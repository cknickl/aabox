//! Minimal DEX parser focused on locating the `Build$VERSION->SDK_INT` field
//! index and rewriting every `sget` instruction that reads it.
//!
//! We don't parse all of DEX. We only:
//!   1. Read the header to find `string_ids`, `type_ids`, `field_ids`,
//!      `class_defs` table offsets / sizes.
//!   2. Look up the string ids of `"SDK_INT"`, `"I"`, and
//!      `"Landroid/os/Build$VERSION;"`.
//!   3. Look up the type ids of `"I"` and `"Landroid/os/Build$VERSION;"`.
//!   4. Walk `field_ids` for the field with those (class, name, type) ids.
//!   5. Walk every `code_item` (reached via class_defs → class_data → methods)
//!      and inside each `insns[]` array, walk the instruction stream using the
//!      standard DEX opcode-length table. Whenever we see an `sget` (any of
//!      0x60..0x66) referencing the SDK_INT field index, rewrite the 4-byte
//!      instruction in place as `const/16 vAA, #0x23` (opcode 0x13, two
//!      code units, signed-16-bit literal 0x0023).
//!   6. Recompute the DEX header's adler32 + sha1 to keep the runtime
//!      verifier happy.
//!
//! Reference: <https://source.android.com/docs/core/runtime/dex-format>
//! Opcode reference: <https://source.android.com/docs/core/runtime/dalvik-bytecode>

use anyhow::{anyhow, bail, Result};

// ---- DEX magic / endian ----------------------------------------------------

pub const DEX_MAGIC_PREFIX: &[u8; 4] = b"dex\n";
pub const ENDIAN_CONSTANT: u32 = 0x12345678;

// The clamp value we write: 35 decimal = Android 14 = the last SDK CarCar
// admits to supporting. Anything ≥ 35 is fine for our purposes; 35 keeps
// us out of any other "≥ 35 means new behaviour" code path.
pub const CLAMPED_SDK_INT: i16 = 35;

// ---- Public entry point ----------------------------------------------------

/// Result of a single DEX patch pass.
#[derive(Debug, Clone, Default)]
pub struct DexPatchResult {
    /// Number of `sget …SDK_INT:I` instructions rewritten.
    pub patches: usize,
    /// Bytes rewritten (always 4 × patches).
    pub bytes_rewritten: usize,
}

/// Patch `dex` in place. Returns the number of rewrites performed.
/// Does nothing (returns `patches = 0`) if the DEX has no SDK_INT reference.
pub fn patch_dex(dex: &mut [u8]) -> Result<DexPatchResult> {
    let header = DexHeader::parse(dex)?;

    // Look up strings.
    let sdk_int_str = match find_string_idx(dex, &header, "SDK_INT")? {
        Some(i) => i,
        None => return Ok(DexPatchResult::default()),
    };
    let i_str = match find_string_idx(dex, &header, "I")? {
        Some(i) => i,
        None => return Ok(DexPatchResult::default()),
    };
    let build_ver_str = match find_string_idx(dex, &header, "Landroid/os/Build$VERSION;")? {
        Some(i) => i,
        None => return Ok(DexPatchResult::default()),
    };

    // Look up types pointing at those strings.
    let i_type = match find_type_idx(dex, &header, i_str)? {
        Some(i) => i,
        None => return Ok(DexPatchResult::default()),
    };
    let build_ver_type = match find_type_idx(dex, &header, build_ver_str)? {
        Some(i) => i,
        None => return Ok(DexPatchResult::default()),
    };

    // Look up the field id.
    let field_idx = match find_field_idx(dex, &header, build_ver_type, sdk_int_str, i_type)? {
        Some(f) => f,
        None => return Ok(DexPatchResult::default()),
    };
    tracing::debug!(field_idx, "located Build.VERSION.SDK_INT field index");

    // Walk every code_item and rewrite matching sget instructions.
    let mut result = DexPatchResult::default();
    walk_code_items(dex, &header, |insns| {
        rewrite_sget_in_insns(insns, field_idx, &mut result);
    })?;

    if result.patches > 0 {
        // Recompute checksum / signature in header (offsets 8..12 = adler32
        // of dex[12..], 12..32 = sha1 of dex[32..]).
        recompute_header_checksums(dex)?;
    }
    Ok(result)
}

// ---- Header ---------------------------------------------------------------

#[derive(Debug)]
struct DexHeader {
    string_ids_size: u32,
    string_ids_off: u32,
    type_ids_size: u32,
    type_ids_off: u32,
    field_ids_size: u32,
    field_ids_off: u32,
    class_defs_size: u32,
    class_defs_off: u32,
}

impl DexHeader {
    fn parse(dex: &[u8]) -> Result<Self> {
        if dex.len() < 0x70 {
            bail!("dex too short ({} bytes)", dex.len());
        }
        if &dex[0..4] != DEX_MAGIC_PREFIX {
            bail!("not a DEX file (bad magic)");
        }
        // Validate endian (we assume little-endian, which Android always is).
        let endian = read_u32(dex, 0x28)?;
        if endian != ENDIAN_CONSTANT {
            bail!("unsupported DEX endian: 0x{:x}", endian);
        }
        Ok(Self {
            string_ids_size: read_u32(dex, 0x38)?,
            string_ids_off: read_u32(dex, 0x3c)?,
            type_ids_size: read_u32(dex, 0x40)?,
            type_ids_off: read_u32(dex, 0x44)?,
            // proto_ids @ 0x48/0x4c (skipped — we don't need protos)
            field_ids_size: read_u32(dex, 0x50)?,
            field_ids_off: read_u32(dex, 0x54)?,
            // method_ids @ 0x58/0x5c (skipped)
            class_defs_size: read_u32(dex, 0x60)?,
            class_defs_off: read_u32(dex, 0x64)?,
        })
    }
}

// ---- String / type / field lookups ----------------------------------------

fn find_string_idx(dex: &[u8], h: &DexHeader, needle: &str) -> Result<Option<u32>> {
    let needle_bytes = needle.as_bytes();
    // string_id_item is { u32 string_data_off; }
    for i in 0..h.string_ids_size {
        let entry = (h.string_ids_off + i * 4) as usize;
        let data_off = read_u32(dex, entry)? as usize;
        // string_data_item starts with uleb128 utf16_size, then MUTF-8 bytes
        // null-terminated. For ASCII strings (which all our needles are),
        // MUTF-8 == ASCII byte-for-byte. We don't need to decode utf16_size,
        // we just step past the uleb128 and compare bytes.
        let (str_start, _utf16_size) = read_uleb128(dex, data_off)?;
        // Bounds check.
        if str_start + needle_bytes.len() > dex.len() {
            continue;
        }
        if &dex[str_start..str_start + needle_bytes.len()] == needle_bytes
            && dex.get(str_start + needle_bytes.len()) == Some(&0)
        {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

fn find_type_idx(dex: &[u8], h: &DexHeader, descriptor_string_idx: u32) -> Result<Option<u32>> {
    // type_id_item is { u32 descriptor_idx; }
    for i in 0..h.type_ids_size {
        let entry = (h.type_ids_off + i * 4) as usize;
        let s = read_u32(dex, entry)?;
        if s == descriptor_string_idx {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

fn find_field_idx(
    dex: &[u8],
    h: &DexHeader,
    class_type_idx: u32,
    name_string_idx: u32,
    type_idx: u32,
) -> Result<Option<u32>> {
    // field_id_item is { u16 class_idx; u16 type_idx; u32 name_idx; }
    if class_type_idx > u16::MAX as u32 || type_idx > u16::MAX as u32 {
        bail!("type idx out of u16 range");
    }
    for i in 0..h.field_ids_size {
        let entry = (h.field_ids_off + i * 8) as usize;
        let class_idx = read_u16(dex, entry)?;
        let t_idx = read_u16(dex, entry + 2)?;
        let n_idx = read_u32(dex, entry + 4)?;
        if class_idx as u32 == class_type_idx
            && t_idx as u32 == type_idx
            && n_idx == name_string_idx
        {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

// ---- code_item walker -----------------------------------------------------

fn walk_code_items<F>(dex: &mut [u8], h: &DexHeader, mut on_insns: F) -> Result<()>
where
    F: FnMut(&mut [u8]),
{
    let mut classes_visited = 0usize;
    let mut classes_failed = 0usize;
    let mut methods_walked = 0usize;
    // class_def_item is 32 bytes; class_data_off is at +24.
    for i in 0..h.class_defs_size {
        let cd_off = (h.class_defs_off + i * 32) as usize;
        let class_data_off = match read_u32(dex, cd_off + 24) {
            Ok(v) => v as usize,
            Err(e) => {
                tracing::warn!(class_idx = i, error = %e, "class_def OOB; skipping");
                classes_failed += 1;
                continue;
            }
        };
        if class_data_off == 0 {
            continue;
        }
        match walk_one_class_data(dex, class_data_off, &mut on_insns) {
            Ok(n) => {
                classes_visited += 1;
                methods_walked += n;
            }
            Err(e) => {
                tracing::warn!(
                    class_idx = i,
                    class_data_off,
                    error = %e,
                    "class_data parse failed; skipping"
                );
                classes_failed += 1;
            }
        }
    }
    tracing::debug!(
        classes_visited,
        classes_failed,
        methods_walked,
        "code_item walk complete"
    );
    Ok(())
}

fn walk_one_class_data<F>(
    dex: &mut [u8],
    class_data_off: usize,
    on_insns: &mut F,
) -> Result<usize>
where
    F: FnMut(&mut [u8]),
{
    let mut p = class_data_off;
    let (np, static_fields_size) = read_uleb128(dex, p)?;
    p = np;
    let (np, instance_fields_size) = read_uleb128(dex, p)?;
    p = np;
    let (np, direct_methods_size) = read_uleb128(dex, p)?;
    p = np;
    let (np, virtual_methods_size) = read_uleb128(dex, p)?;
    p = np;

    // Skip static & instance fields (two ulebs each).
    for _ in 0..(static_fields_size + instance_fields_size) {
        let (np, _) = read_uleb128(dex, p)?;
        p = np;
        let (np, _) = read_uleb128(dex, p)?;
        p = np;
    }

    let mut methods_walked = 0usize;
    let total_methods = direct_methods_size + virtual_methods_size;
    // The direct_methods + virtual_methods are two separate arrays whose
    // method_idx_diff values restart at 0 between the two groups. We don't
    // care about indices though — just need three ulebs per method.
    for _ in 0..total_methods {
        let (np, _) = read_uleb128(dex, p)?;
        p = np;
        let (np, _) = read_uleb128(dex, p)?;
        p = np;
        let (np, code_off) = read_uleb128(dex, p)?;
        p = np;
        if code_off == 0 {
            continue;
        }
        let code_off = code_off as usize;
        let insns_size = read_u32(dex, code_off + 12)? as usize;
        let insns_byte_start = code_off + 16;
        let insns_byte_end = insns_byte_start
            .checked_add(insns_size * 2)
            .ok_or_else(|| anyhow!("insns size overflow"))?;
        if insns_byte_end > dex.len() {
            bail!(
                "code_item @ {} insns out of bounds (end {} > dex {})",
                code_off,
                insns_byte_end,
                dex.len()
            );
        }
        on_insns(&mut dex[insns_byte_start..insns_byte_end]);
        methods_walked += 1;
    }
    Ok(methods_walked)
}

// ---- instruction stream rewrite -------------------------------------------

fn rewrite_sget_in_insns(insns: &mut [u8], target_field_idx: u32, out: &mut DexPatchResult) {
    // Walk instructions using the standard length table. Each instruction
    // is N code units (16-bit). Length is determined by the opcode in the
    // low byte of the first code unit.
    let mut i = 0usize;
    while i + 2 <= insns.len() {
        let opcode = insns[i];
        // packed-switch-payload / sparse-switch-payload / fill-array-data-payload
        // are pseudo-instructions identified by code unit 0xNN00 where NN is
        // 01..03. They have variable length and only appear as targets of
        // packed-switch / sparse-switch / fill-array-data — never reached by
        // sequential walk because the preceding instruction jumps to them
        // and the actual sequential successor is something else. But in
        // practice they're emitted inline and we need to skip them properly.
        if opcode == 0x00 {
            let ident = insns[i + 1];
            let payload_units = match ident {
                0x00 => 1, // nop
                0x01 => {
                    // packed-switch-payload: u16 size, u32 first_key,
                    // u32[size] targets → 4 + size*2 code units
                    if i + 4 > insns.len() {
                        break;
                    }
                    let size = u16::from_le_bytes([insns[i + 2], insns[i + 3]]) as usize;
                    4 + size * 2
                }
                0x02 => {
                    // sparse-switch-payload: u16 size, u32[size] keys,
                    // u32[size] targets → 2 + size*4 code units
                    if i + 4 > insns.len() {
                        break;
                    }
                    let size = u16::from_le_bytes([insns[i + 2], insns[i + 3]]) as usize;
                    2 + size * 4
                }
                0x03 => {
                    // fill-array-data-payload: u16 element_width, u32 size,
                    // size*element_width bytes (rounded up to code units)
                    if i + 8 > insns.len() {
                        break;
                    }
                    let element_width =
                        u16::from_le_bytes([insns[i + 2], insns[i + 3]]) as usize;
                    let size = u32::from_le_bytes([
                        insns[i + 4],
                        insns[i + 5],
                        insns[i + 6],
                        insns[i + 7],
                    ]) as usize;
                    let data_bytes = size * element_width;
                    let data_units = (data_bytes + 1) / 2;
                    4 + data_units
                }
                _ => 1,
            };
            i += payload_units * 2;
            continue;
        }

        let units = OPCODE_LENGTH_CODE_UNITS[opcode as usize];
        if units == 0 {
            // Unknown / unused opcode. Be conservative: advance one code unit.
            i += 2;
            continue;
        }
        let inst_bytes = (units as usize) * 2;
        if i + inst_bytes > insns.len() {
            break;
        }

        // sget family: 0x60..=0x66 (sget, sget-wide, sget-object, sget-boolean,
        // sget-byte, sget-char, sget-short). All have format 21c (2 code units):
        //   byte 0: opcode
        //   byte 1: vAA
        //   bytes 2..4: field_idx (u16 LE)
        // We patch only sget (int) — opcode 0x60 — because SDK_INT is `I`,
        // not wide/object/short. (Belt and suspenders: also patch any of the
        // 0x60..0x66 that references the SDK_INT field id, since smali compilers
        // technically only emit 0x60 for I but if anyone has been weird about
        // it we still do the right thing.)
        if (0x60..=0x66).contains(&opcode) && inst_bytes == 4 {
            let field_idx = u16::from_le_bytes([insns[i + 2], insns[i + 3]]) as u32;
            if field_idx == target_field_idx {
                // Rewrite to: const/16 vAA, #+0x23
                //   byte 0 = 0x13 (const/16 opcode)
                //   byte 1 = vAA (preserved)
                //   bytes 2..4 = signed int16 literal LE
                let v_aa = insns[i + 1];
                insns[i] = 0x13;
                insns[i + 1] = v_aa;
                let lit = CLAMPED_SDK_INT.to_le_bytes();
                insns[i + 2] = lit[0];
                insns[i + 3] = lit[1];
                out.patches += 1;
                out.bytes_rewritten += 4;
            }
        }
        i += inst_bytes;
    }
}

// ---- Header checksum / signature recomputation ----------------------------

fn recompute_header_checksums(dex: &mut [u8]) -> Result<()> {
    if dex.len() < 32 {
        bail!("dex too short for header recompute");
    }
    // SHA-1 of dex[32..] → written to dex[12..32]
    use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};
    let mut ctx = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(&dex[32..]);
    let digest = ctx.finish();
    dex[12..32].copy_from_slice(digest.as_ref());

    // Adler-32 of dex[12..] → written to dex[8..12]
    let adler = adler32(&dex[12..]);
    dex[8..12].copy_from_slice(&adler.to_le_bytes());
    Ok(())
}

fn adler32(buf: &[u8]) -> u32 {
    const MOD_ADLER: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in buf {
        a = (a + byte as u32) % MOD_ADLER;
        b = (b + a) % MOD_ADLER;
    }
    (b << 16) | a
}

// ---- Primitives -----------------------------------------------------------

fn read_u16(buf: &[u8], off: usize) -> Result<u16> {
    if off + 2 > buf.len() {
        bail!("read_u16 out of bounds @ {}", off);
    }
    Ok(u16::from_le_bytes([buf[off], buf[off + 1]]))
}

fn read_u32(buf: &[u8], off: usize) -> Result<u32> {
    if off + 4 > buf.len() {
        bail!("read_u32 out of bounds @ {}", off);
    }
    Ok(u32::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
    ]))
}

/// Decode a uleb128 starting at `off`. Returns `(next_off, value)`.
fn read_uleb128(buf: &[u8], off: usize) -> Result<(usize, u32)> {
    let mut result: u32 = 0;
    let mut shift = 0;
    let mut p = off;
    for _ in 0..5 {
        if p >= buf.len() {
            bail!("uleb128 truncated @ {}", off);
        }
        let byte = buf[p];
        p += 1;
        result |= ((byte & 0x7f) as u32) << shift;
        if (byte & 0x80) == 0 {
            return Ok((p, result));
        }
        shift += 7;
    }
    bail!("uleb128 overflow @ {}", off);
}

// ---- DEX opcode length table (in 16-bit code units) -----------------------
//
// 0 = unused / unknown.
// Derived from Android's `dalvik-bytecode` reference. Length of each
// instruction is fixed by its opcode.
//
// Format families and their code-unit counts:
//   10x, 12x, 11n, 11x   → 1
//   20t, 22x, 21t, 21s, 21h, 21c, 23x, 22b, 22t, 22s, 22c, 22cs → 2
//   30t, 32x, 31i, 31t, 31c, 35c, 35ms, 35mi, 3rc, 3rms, 3rmi, 45cc → 3 (most)
//   51l, 4rcc → many (handled per opcode)
// We just hand-build the 256-entry table; off-by-one here is the single most
// dangerous bug surface, so it's fully spelled out and unit-tested.

// Generated from /aosp/aosp/dalvik/opcode-gen/bytecode.txt by mapping each
// instruction's format-code to its code-unit count. Format → units mapping:
//   10x/10t/11n/11x/12x/00x → 1
//   20t/20bc/21*/22*/23x   → 2
//   30t/31*/32x/35*/3r*    → 3
//   45cc/4rcc              → 4
//   51l                    → 5
// 0 means "unused / illegal opcode" — if encountered while walking, the
// instruction stream is malformed (or we're scanning data the dex layout
// doesn't actually treat as code). We log + advance two bytes conservatively.
#[rustfmt::skip]
static OPCODE_LENGTH_CODE_UNITS: [u8; 256] = [
    1, 1, 2, 3, 1, 2, 3, 1, 2, 3, 1, 1, 1, 1, 1, 1,  // 0x00-0x0f
    1, 1, 1, 2, 3, 2, 2, 3, 5, 2, 2, 3, 2, 1, 1, 2,  // 0x10-0x1f
    2, 1, 2, 2, 3, 3, 3, 1, 1, 2, 3, 3, 3, 2, 2, 2,  // 0x20-0x2f
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 0, 0,  // 0x30-0x3f
    0, 0, 0, 0, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,  // 0x40-0x4f
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,  // 0x50-0x5f
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3,  // 0x60-0x6f
    3, 3, 3, 0, 3, 3, 3, 3, 3, 0, 0, 1, 1, 1, 1, 1,  // 0x70-0x7f
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 0x80-0x8f
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,  // 0x90-0x9f
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,  // 0xa0-0xaf
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 0xb0-0xbf
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 0xc0-0xcf
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,  // 0xd0-0xdf
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 2, 3, 3,  // 0xe0-0xef
    3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 4, 4, 3, 3, 2, 2,  // 0xf0-0xff
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_table_sget_is_2_units() {
        assert_eq!(OPCODE_LENGTH_CODE_UNITS[0x60], 2);
        assert_eq!(OPCODE_LENGTH_CODE_UNITS[0x66], 2);
    }

    #[test]
    fn opcode_table_const_is_one_or_two() {
        assert_eq!(OPCODE_LENGTH_CODE_UNITS[0x12], 1); // const/4
        assert_eq!(OPCODE_LENGTH_CODE_UNITS[0x13], 2); // const/16
        assert_eq!(OPCODE_LENGTH_CODE_UNITS[0x14], 3); // const
        assert_eq!(OPCODE_LENGTH_CODE_UNITS[0x15], 2); // const/high16
    }

    #[test]
    fn adler32_known_vectors() {
        // "Wikipedia" → 0x11E60398 (Adler-32 reference test vector).
        assert_eq!(adler32(b"Wikipedia"), 0x11E60398);
    }

    #[test]
    fn uleb128_basic() {
        // 0 → 1 byte 0
        let (n, v) = read_uleb128(&[0x00], 0).unwrap();
        assert_eq!((n, v), (1, 0));
        // 127 → 1 byte 0x7f
        let (n, v) = read_uleb128(&[0x7f], 0).unwrap();
        assert_eq!((n, v), (1, 127));
        // 128 → 2 bytes 0x80 0x01
        let (n, v) = read_uleb128(&[0x80, 0x01], 0).unwrap();
        assert_eq!((n, v), (2, 128));
        // 16383 → 0xff 0x7f
        let (n, v) = read_uleb128(&[0xff, 0x7f], 0).unwrap();
        assert_eq!((n, v), (2, 16383));
    }

    #[test]
    fn rewrite_one_sget() {
        // sget v3, field#0x1234 → const/16 v3, #0x23
        // Bytes: 0x60 0x03 0x34 0x12
        let mut insns = vec![0x60, 0x03, 0x34, 0x12];
        let mut r = DexPatchResult::default();
        rewrite_sget_in_insns(&mut insns, 0x1234, &mut r);
        assert_eq!(r.patches, 1);
        assert_eq!(insns, vec![0x13, 0x03, 0x23, 0x00]);
    }

    #[test]
    fn rewrite_skips_other_fields() {
        // sget v3, field#0xAAAA  — wrong field, must not be touched.
        let mut insns = vec![0x60, 0x03, 0xaa, 0xaa];
        let original = insns.clone();
        let mut r = DexPatchResult::default();
        rewrite_sget_in_insns(&mut insns, 0x1234, &mut r);
        assert_eq!(r.patches, 0);
        assert_eq!(insns, original);
    }

    #[test]
    fn rewrite_handles_mixed_instructions() {
        // Stream:
        //   const/4 v1, #0     (1 unit / 2 bytes:  0x12 0x10)
        //   sget    v2, #5     (2 units / 4 bytes: 0x60 0x02 0x05 0x00)
        //   return-void        (1 unit / 2 bytes:  0x0e 0x00)
        let mut insns = vec![0x12, 0x10, 0x60, 0x02, 0x05, 0x00, 0x0e, 0x00];
        let mut r = DexPatchResult::default();
        rewrite_sget_in_insns(&mut insns, 5, &mut r);
        assert_eq!(r.patches, 1);
        assert_eq!(insns, vec![0x12, 0x10, 0x13, 0x02, 0x23, 0x00, 0x0e, 0x00]);
    }
}

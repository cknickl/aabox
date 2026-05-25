//! Annex-B NAL-unit splitter.
//!
//! An H.264 elementary stream in Annex-B form is a concatenation of
//! `[start_code][NAL]` sequences. The start code is `00 00 00 01` or
//! `00 00 01`. NAL bytes do not contain `00 00 00` or `00 00 01` thanks to
//! the emulation-prevention byte (`00 00 03 xx`), so naive scanning is safe.
//!
//! This module provides:
//!   - [`split_nals`]: lazy iterator over `(start_code, nal_bytes)` slices.
//!   - [`group_access_units`]: group consecutive NALs into "access units"
//!     (everything between two slice-start boundaries, plus the leading
//!     parameter sets). For our test-pattern source one AU = "one frame on
//!     the wire", which is the natural fragmentation unit for AAP.
//!   - [`nal_unit_type`]: extract the 5-bit `nal_unit_type` from the NAL
//!     header byte.

/// Index of every start code in the bitstream. Each returned tuple is
/// `(start, prefix_len)` where `prefix_len` is 3 (`00 00 01`) or 4
/// (`00 00 00 01`).
fn start_code_offsets(buf: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if i + 4 <= buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                out.push((i, 4));
                i += 4;
                continue;
            }
            if buf[i + 2] == 1 {
                out.push((i, 3));
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Iterate over `(start_code_with_prefix, nal_body)` slices in `buf`. The
/// start-code prefix is **included** in the returned slice's first element
/// so callers that want to emit Annex-B back out can `concat()` losslessly.
pub fn split_nals(buf: &[u8]) -> Vec<&[u8]> {
    let codes = start_code_offsets(buf);
    let mut out = Vec::with_capacity(codes.len());
    for i in 0..codes.len() {
        let (start, _) = codes[i];
        let end = codes.get(i + 1).map(|c| c.0).unwrap_or(buf.len());
        out.push(&buf[start..end]);
    }
    out
}

/// The H.264 `nal_unit_type` field from a NAL header byte (the byte
/// immediately after the start code). Returns 0 on empty input.
pub fn nal_unit_type(nal: &[u8]) -> u8 {
    // The NAL starts with the start code (3 or 4 bytes), then the header byte.
    for i in 0..nal.len().saturating_sub(3) {
        if nal[i] == 0 && nal[i + 1] == 0 {
            if nal[i + 2] == 1 && i + 3 < nal.len() {
                return nal[i + 3] & 0x1f;
            }
            if nal[i + 2] == 0 && i + 3 < nal.len() && nal[i + 3] == 1 && i + 4 < nal.len() {
                return nal[i + 4] & 0x1f;
            }
        }
    }
    0
}

/// H.264 NAL unit types we care about.
pub mod nut {
    pub const SLICE_NON_IDR: u8 = 1;
    pub const SLICE_IDR: u8 = 5;
    pub const SEI: u8 = 6;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const AUD: u8 = 9;
}

/// True if this NAL begins a coded slice (IDR or non-IDR). For Annex-B
/// streams, a sequence of NALs that share a common access unit ends just
/// before the next slice NAL (or at end of stream).
pub fn is_slice(nut_val: u8) -> bool {
    matches!(nut_val, nut::SLICE_NON_IDR | nut::SLICE_IDR)
}

/// Group `split_nals` output into access units: each returned `Vec<u8>` is
/// the concatenation of NALs that should be sent in a single AAP frame
/// (one frame on the wire per displayed picture). Parameter sets (SPS, PPS)
/// and SEI / AUD NALs at the head of the stream get folded into the
/// following access unit, ensuring the decoder always has them before its
/// first IDR.
pub fn group_access_units(buf: &[u8]) -> Vec<Vec<u8>> {
    let nals = split_nals(buf);
    let mut aus: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    for n in nals {
        let nut_val = nal_unit_type(n);
        if is_slice(nut_val) {
            // Append this slice to the current AU and flush.
            current.extend_from_slice(n);
            aus.push(std::mem::take(&mut current));
        } else {
            // SPS / PPS / SEI / AUD — prepend to the next AU.
            current.extend_from_slice(n);
        }
    }
    // Anything trailing without a slice — emit as its own AU so we don't drop bytes.
    if !current.is_empty() {
        aus.push(current);
    }
    aus
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_recognises_three_and_four_byte_start_codes() {
        // SPS (3-byte start), PPS (3-byte start), IDR (4-byte start)
        let buf = [
            0, 0, 1, 0x67, 0xAA, // SPS
            0, 0, 1, 0x68, 0xBB, // PPS
            0, 0, 0, 1, 0x65, 0xCC, 0xDD, // IDR
        ];
        let nals = split_nals(&buf);
        assert_eq!(nals.len(), 3);
        assert_eq!(nal_unit_type(nals[0]), nut::SPS);
        assert_eq!(nal_unit_type(nals[1]), nut::PPS);
        assert_eq!(nal_unit_type(nals[2]), nut::SLICE_IDR);
    }

    #[test]
    fn access_unit_grouping_collapses_param_sets_with_following_idr() {
        let buf = [
            0, 0, 0, 1, 0x67, 0xAA, // SPS
            0, 0, 0, 1, 0x68, 0xBB, // PPS
            0, 0, 0, 1, 0x65, 0xCC, // IDR
            0, 0, 0, 1, 0x61, 0xDD, // non-IDR slice
        ];
        let aus = group_access_units(&buf);
        assert_eq!(aus.len(), 2);
        // First AU starts with SPS, contains PPS + IDR slice.
        assert_eq!(aus[0][..4], [0, 0, 0, 1]);
        assert_eq!(aus[0][4] & 0x1f, nut::SPS);
        // Second AU = the non-IDR slice on its own.
        assert_eq!(aus[1][4] & 0x1f, nut::SLICE_NON_IDR);
    }

    #[test]
    fn empty_buffer_yields_no_nals() {
        assert!(split_nals(&[]).is_empty());
        assert!(group_access_units(&[]).is_empty());
    }
}

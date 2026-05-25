//! Shared test helpers: a hand-rolled minimal DEX builder used by
//! `dex_fixture.rs` and `end_to_end.rs`.

#![allow(dead_code)]

/// Build a tiny, structurally valid DEX containing exactly one class with
/// one method whose code body is:
///
///   sget v0, Build$VERSION.SDK_INT
///   return-void
///
/// Returns the dex bytes.
pub fn build_synthetic_dex() -> Vec<u8> {
    let mut d = DexBuilder::new();

    let str_i = d.add_string("I");
    let str_build_ver = d.add_string("Landroid/os/Build$VERSION;");
    let str_sdk_int = d.add_string("SDK_INT");
    let str_main = d.add_string("LMain;");
    let _str_init = d.add_string("<init>");
    let _str_v = d.add_string("V");

    let type_i = d.add_type(str_i);
    let type_build_ver = d.add_type(str_build_ver);
    let _type_main = d.add_type(str_main);

    let field_sdk_int = d.add_field(type_build_ver, type_i, str_sdk_int);

    d.finish_with_one_class_with_sget(field_sdk_int)
}

struct DexBuilder {
    strings: Vec<String>,
    types: Vec<u32>,
    fields: Vec<(u16, u16, u32)>,
}

impl DexBuilder {
    fn new() -> Self {
        Self {
            strings: Vec::new(),
            types: Vec::new(),
            fields: Vec::new(),
        }
    }

    fn add_string(&mut self, s: &str) -> u32 {
        self.strings.push(s.to_string());
        (self.strings.len() - 1) as u32
    }
    fn add_type(&mut self, descriptor_idx: u32) -> u32 {
        self.types.push(descriptor_idx);
        (self.types.len() - 1) as u32
    }
    fn add_field(&mut self, class_t: u32, type_t: u32, name_s: u32) -> u32 {
        self.fields.push((class_t as u16, type_t as u16, name_s));
        (self.fields.len() - 1) as u32
    }

    fn finish_with_one_class_with_sget(self, field_idx: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(std::iter::repeat(0u8).take(0x70)); // header placeholder

        let string_ids_off = out.len() as u32;
        let string_ids_size = self.strings.len() as u32;
        let string_ids_table_start = out.len();
        for _ in 0..self.strings.len() {
            out.extend_from_slice(&[0u8; 4]);
        }

        let type_ids_off = out.len() as u32;
        let type_ids_size = self.types.len() as u32;
        for &descriptor_idx in &self.types {
            out.extend_from_slice(&descriptor_idx.to_le_bytes());
        }

        let field_ids_off = out.len() as u32;
        let field_ids_size = self.fields.len() as u32;
        for &(class_idx, type_idx, name_idx) in &self.fields {
            out.extend_from_slice(&class_idx.to_le_bytes());
            out.extend_from_slice(&type_idx.to_le_bytes());
            out.extend_from_slice(&name_idx.to_le_bytes());
        }

        let class_defs_off = out.len() as u32;
        let class_defs_size = 1u32;
        let class_def_start = out.len();
        out.extend(std::iter::repeat(0u8).take(32));

        // code_item alignment (4 bytes).
        while out.len() % 4 != 0 {
            out.push(0);
        }
        let code_item_off = out.len() as u32;
        out.extend_from_slice(&1u16.to_le_bytes()); // registers_size
        out.extend_from_slice(&0u16.to_le_bytes()); // ins_size
        out.extend_from_slice(&0u16.to_le_bytes()); // outs_size
        out.extend_from_slice(&0u16.to_le_bytes()); // tries_size
        out.extend_from_slice(&0u32.to_le_bytes()); // debug_info_off
        out.extend_from_slice(&3u32.to_le_bytes()); // insns_size = 3 code units
        // sget v0, field#field_idx
        out.push(0x60);
        out.push(0x00);
        out.extend_from_slice(&(field_idx as u16).to_le_bytes());
        // return-void
        out.push(0x0e);
        out.push(0x00);

        let class_data_off = out.len() as u32;
        push_uleb128(&mut out, 0); // static fields
        push_uleb128(&mut out, 0); // instance fields
        push_uleb128(&mut out, 1); // direct methods
        push_uleb128(&mut out, 0); // virtual methods
        push_uleb128(&mut out, 0); // method_idx_diff
        push_uleb128(&mut out, 0); // access_flags
        push_uleb128(&mut out, code_item_off); // code_off

        let mut string_data_offs: Vec<u32> = Vec::with_capacity(self.strings.len());
        for s in &self.strings {
            string_data_offs.push(out.len() as u32);
            push_uleb128(&mut out, s.encode_utf16().count() as u32);
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }

        for (i, off) in string_data_offs.iter().enumerate() {
            let pos = string_ids_table_start + i * 4;
            out[pos..pos + 4].copy_from_slice(&off.to_le_bytes());
        }

        out[class_def_start + 24..class_def_start + 28]
            .copy_from_slice(&class_data_off.to_le_bytes());
        out[class_def_start..class_def_start + 4].copy_from_slice(&2u32.to_le_bytes());
        out[class_def_start + 8..class_def_start + 12]
            .copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        out[class_def_start + 16..class_def_start + 20]
            .copy_from_slice(&0xffff_ffffu32.to_le_bytes());

        let total_size = out.len() as u32;
        let header = build_header(
            total_size,
            string_ids_size,
            string_ids_off,
            type_ids_size,
            type_ids_off,
            field_ids_size,
            field_ids_off,
            class_defs_size,
            class_defs_off,
        );
        out[..0x70].copy_from_slice(&header);
        out
    }
}

fn push_uleb128(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[allow(clippy::too_many_arguments)]
fn build_header(
    total_size: u32,
    string_ids_size: u32,
    string_ids_off: u32,
    type_ids_size: u32,
    type_ids_off: u32,
    field_ids_size: u32,
    field_ids_off: u32,
    class_defs_size: u32,
    class_defs_off: u32,
) -> [u8; 0x70] {
    let mut h = [0u8; 0x70];
    h[..8].copy_from_slice(b"dex\n035\0");
    h[0x20..0x24].copy_from_slice(&total_size.to_le_bytes());
    h[0x24..0x28].copy_from_slice(&0x70u32.to_le_bytes());
    h[0x28..0x2c].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    h[0x38..0x3c].copy_from_slice(&string_ids_size.to_le_bytes());
    h[0x3c..0x40].copy_from_slice(&string_ids_off.to_le_bytes());
    h[0x40..0x44].copy_from_slice(&type_ids_size.to_le_bytes());
    h[0x44..0x48].copy_from_slice(&type_ids_off.to_le_bytes());
    h[0x48..0x4c].copy_from_slice(&0u32.to_le_bytes()); // proto_ids size
    h[0x4c..0x50].copy_from_slice(&0u32.to_le_bytes()); // proto_ids off
    h[0x50..0x54].copy_from_slice(&field_ids_size.to_le_bytes());
    h[0x54..0x58].copy_from_slice(&field_ids_off.to_le_bytes());
    h[0x58..0x5c].copy_from_slice(&0u32.to_le_bytes()); // method_ids size
    h[0x5c..0x60].copy_from_slice(&0u32.to_le_bytes()); // method_ids off
    h[0x60..0x64].copy_from_slice(&class_defs_size.to_le_bytes());
    h[0x64..0x68].copy_from_slice(&class_defs_off.to_le_bytes());
    h
}

//! Reader for pre-XML (5.6 and earlier) binary `.CT` tables.
//!
//! Converts the legacy binary table format to XML. These tables are a
//! flat little-endian record stream rather than XML, and they are common: 576
//! of the 4350 tables in the bundled corpus are in this format.
//!
//! # Magic string
//!
//! The header is 11 bytes of magic, but the value is **not** checked. The
//! reference reader comments its own check out, because rebranded forks ship
//! tables with their own magic (`SpuceEngine`, `DAEngine\0\0\0`, ...). What
//! actually identifies the format is a plausible version and record count in
//! the two dwords that follow.

use crate::ct_types::{CheatEntry, CheatTable, VariableType};
use anyhow::{bail, Result};

const MAGIC_LEN: usize = 11;
/// Tables older than this are refused (open and resave them in 5.6+).
const MIN_VERSION: u32 = 6;
/// Guards against reading noise as a huge record count.
const MAX_RECORDS: u32 = 100_000;

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn u8(&mut self) -> Result<u8> {
        if self.remaining() < 1 {
            bail!("unexpected end of legacy table");
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn u32(&mut self) -> Result<u32> {
        if self.remaining() < 4 {
            bail!("unexpected end of legacy table");
        }
        let v = u32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Ok(v)
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        if self.remaining() < n {
            bail!("unexpected end of legacy table");
        }
        self.pos += n;
        Ok(())
    }

    /// A dword-length-prefixed string. These are ANSI, so decode leniently:
    /// many community tables contain Windows-1252 bytes.
    fn string32(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        if len > self.remaining() {
            bail!("legacy string length {} exceeds remaining bytes", len);
        }
        let s = decode_ansi(&self.data[self.pos..self.pos + len]);
        self.pos += len;
        Ok(s)
    }

    /// A byte-length-prefixed string, used by the code-record section.
    fn string8(&mut self) -> Result<String> {
        let len = self.u8()? as usize;
        if len > self.remaining() {
            bail!("legacy string length {} exceeds remaining bytes", len);
        }
        let s = decode_ansi(&self.data[self.pos..self.pos + len]);
        self.pos += len;
        Ok(s)
    }
}

/// Decodes bytes that are UTF-8 if possible, else Windows-1252.
///
/// Legacy tables predate UTF-8, and the corpus contains plenty of
/// Latin-1 accented descriptions that would otherwise fail to decode.
pub fn decode_ansi(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes
            .iter()
            .map(|b| windows_1252_char(*b))
            .collect::<String>(),
    }
}

/// Maps a Windows-1252 byte to its Unicode code point.
///
/// Only 0x80-0x9F differ from Latin-1; the rest map straight through.
fn windows_1252_char(b: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
        '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

/// Maps the legacy variable-type enum onto the current one.
fn old_var_type(i: u8) -> VariableType {
    match i {
        0 => VariableType::Byte,
        1 => VariableType::Word,
        2 => VariableType::Dword,
        3 => VariableType::Float,
        4 => VariableType::Double,
        5 => VariableType::Binary,
        6 => VariableType::Qword,
        7 => VariableType::String,
        8 => VariableType::ByteArray,
        10 => VariableType::Custom("Custom".to_string()),
        255 => VariableType::AutoAssembler,
        _ => VariableType::Dword,
    }
}

/// True when `data` looks like a legacy binary table.
///
/// Checks the two dwords after the 11-byte magic rather than the magic itself,
/// since rebranded forks use their own magic strings.
pub fn looks_like_legacy(data: &[u8]) -> bool {
    if data.len() < MAGIC_LEN + 8 {
        return false;
    }
    // XML and protected .CETRAINER files are handled elsewhere.
    if data.starts_with(b"<?xml") || data.starts_with(b"<?XML") || data.starts_with(b"<CheatTable")
    {
        return false;
    }
    // The magic must at least be printable, so arbitrary binary is not claimed.
    if !data[..MAGIC_LEN]
        .iter()
        .all(|b| b.is_ascii_graphic() || *b == 0 || *b == b' ')
    {
        return false;
    }

    let version = u32::from_le_bytes(data[MAGIC_LEN..MAGIC_LEN + 4].try_into().unwrap());
    let records = u32::from_le_bytes(data[MAGIC_LEN + 4..MAGIC_LEN + 8].try_into().unwrap());

    (MIN_VERSION..=100).contains(&version) && records <= MAX_RECORDS
}

pub fn parse(data: &[u8]) -> Result<CheatTable> {
    if data.len() < MAGIC_LEN + 8 {
        bail!("file is too small to be a legacy CT table");
    }

    let mut c = Cursor::new(data);
    c.skip(MAGIC_LEN)?;

    let version = c.u32()?;
    if version < MIN_VERSION {
        bail!(
            "legacy table version {} is too old; open and resave it in a 5.6+ table editor",
            version
        );
    }

    let record_count = c.u32()?;
    if record_count > MAX_RECORDS {
        bail!("implausible legacy record count {}", record_count);
    }

    let mut table = CheatTable {
        version: 45,
        ..Default::default()
    };

    // Six fixed groups are materialised, keeping only those actually used.
    let mut groups: Vec<(CheatEntry, bool)> = (1..=6)
        .map(|i| {
            (
                CheatEntry {
                    id: 900_000 + i,
                    description: format!("Group {}", i),
                    is_group_header: true,
                    ..Default::default()
                },
                false,
            )
        })
        .collect();

    let mut top: Vec<CheatEntry> = Vec::new();

    for i in 0..record_count {
        let mut entry = CheatEntry {
            id: i + 1,
            ..Default::default()
        };

        entry.description = c.string32()?;

        // Numeric address, possibly overridden by an interpretable string.
        let addr = c.u32()?;
        let mut address = format!("{:08X}", addr);
        let interpretable = c.string32()?;
        if !interpretable.is_empty() {
            address = interpretable;
        }

        entry.var_type = old_var_type(c.u8()?);
        let _unicode = c.u8()?;
        let _bit_or_length = c.u8()?;
        let _bit_length = c.u32()?;

        let group = c.u8()?;
        let _show_as_hex = c.u8()?;
        let _is_pointer = c.u8()?;

        let pointer_count = c.u32()?;
        if pointer_count as usize > c.remaining() {
            bail!("implausible pointer count {}", pointer_count);
        }

        // Offsets are stored outermost-first here, but the XML `<Offsets>` and
        // this runtime both apply them last-to-first, so keep the stored order.
        for j in 0..pointer_count {
            let base = c.u32()?;
            let offset = c.u32()?;
            entry.offsets.push(format!("{:X}", offset));

            let interp = c.string32()?;
            // The final pointer carries the chain's base address.
            if j == pointer_count - 1 {
                address = if interp.is_empty() {
                    format!("{:08X}", base)
                } else {
                    interp
                };
            }
        }

        entry.address = Some(address);

        let script = c.string32()?;
        if !script.is_empty() {
            entry.assembler_script = Some(script);
        }

        if (1..=6).contains(&group) {
            let idx = (group - 1) as usize;
            groups[idx].1 = true;
            groups[idx].0.children.push(entry);
        } else {
            top.push(entry);
        }
    }

    // Used groups come first, matching the ordering.
    for (group, used) in groups {
        if used {
            table.entries.push(group);
        }
    }
    table.entries.extend(top);

    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal legacy table with one entry.
    fn synth(magic: &[u8], version: u32, description: &str, group: u8) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(magic);
        v.extend_from_slice(&version.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // one record

        v.extend_from_slice(&(description.len() as u32).to_le_bytes());
        v.extend_from_slice(description.as_bytes());
        v.extend_from_slice(&0x0040_1000u32.to_le_bytes()); // address
        v.extend_from_slice(&0u32.to_le_bytes()); // no interpretable address
        v.push(3); // vtSingle
        v.push(0); // unicode
        v.push(0); // bit/length
        v.extend_from_slice(&0u32.to_le_bytes()); // bitlength
        v.push(group);
        v.push(0); // show as hex
        v.push(0); // is pointer
        v.extend_from_slice(&0u32.to_le_bytes()); // pointer count
        v.extend_from_slice(&0u32.to_le_bytes()); // empty script
        v.extend_from_slice(&0u32.to_le_bytes()); // no code records
        v
    }

    #[test]
    fn reads_a_basic_legacy_table() {
        let data = synth(b"CHEATENGINE", 8, "Health", 0);
        assert!(looks_like_legacy(&data));
        let t = parse(&data).unwrap();
        assert_eq!(t.entries.len(), 1);
        assert_eq!(t.entries[0].description, "Health");
        assert_eq!(t.entries[0].var_type, VariableType::Float);
        assert_eq!(t.entries[0].address.as_deref(), Some("00401000"));
    }

    #[test]
    fn rebranded_forks_are_accepted() {
        // The reference reader comments out its own magic check for this reason.
        for magic in [&b"SpuceEngine"[..], &b"DAEngine\0\0\0"[..]] {
            let data = synth(magic, 7, "Ammo", 0);
            assert!(looks_like_legacy(&data), "magic {:?} rejected", magic);
            let t = parse(&data).unwrap();
            assert_eq!(t.entries[0].description, "Ammo");
        }
    }

    #[test]
    fn grouped_entries_become_group_headers() {
        let data = synth(b"CHEATENGINE", 8, "Stamina", 2);
        let t = parse(&data).unwrap();
        assert_eq!(t.entries.len(), 1);
        assert!(t.entries[0].is_group_header);
        assert_eq!(t.entries[0].description, "Group 2");
        assert_eq!(t.entries[0].children.len(), 1);
        assert_eq!(t.entries[0].children[0].description, "Stamina");
    }

    #[test]
    fn xml_and_noise_are_not_claimed() {
        assert!(!looks_like_legacy(b"<?xml version=\"1.0\"?><CheatTable/>"));
        assert!(!looks_like_legacy(&[0xFFu8; 64]));
        assert!(!looks_like_legacy(b"short"));
    }

    #[test]
    fn versions_below_six_are_rejected_with_a_clear_message() {
        let data = synth(b"CHEATENGINE", 3, "Old", 0);
        let err = parse(&data).unwrap_err().to_string();
        assert!(err.contains("too old"), "unexpected error: {}", err);
    }

    #[test]
    fn latin1_descriptions_decode_instead_of_failing() {
        // 0xE9 is 'e-acute' in Windows-1252 and invalid UTF-8 on its own.
        let mut data = Vec::new();
        data.extend_from_slice(b"CHEATENGINE");
        data.extend_from_slice(&8u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        let desc = b"Sant\xE9";
        data.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        data.extend_from_slice(desc);
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&[2, 0, 0]);
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&[0, 0, 0]);
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());

        let t = parse(&data).unwrap();
        assert_eq!(t.entries[0].description, "Santé");
    }

    #[test]
    fn pointer_chains_are_read() {
        let mut v = Vec::new();
        v.extend_from_slice(b"CHEATENGINE");
        v.extend_from_slice(&8u32.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&4u32.to_le_bytes());
        v.extend_from_slice(b"Gold");
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&[2, 0, 0]);
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&[0, 0, 1]);
        v.extend_from_slice(&2u32.to_le_bytes()); // two pointers
        // pointer 0
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0x10u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        // pointer 1 (last: supplies the base address)
        v.extend_from_slice(&0x0040_2000u32.to_le_bytes());
        v.extend_from_slice(&0x24u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // script
        v.extend_from_slice(&0u32.to_le_bytes()); // code records

        let t = parse(&v).unwrap();
        let e = &t.entries[0];
        assert_eq!(e.offsets, vec!["10".to_string(), "24".to_string()]);
        assert_eq!(e.address.as_deref(), Some("00402000"));
    }
}

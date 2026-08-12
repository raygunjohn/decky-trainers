//! Reads the payload `ct2trainer` appended to this executable (see
//! `ct2trainer/src/overlay.rs`).
//!
//! Layout: `[PE][DEFLATE archive][32-byte footer]`. The footer's magic,
//! field order and size must match the writer exactly - the two live in
//! separate crates with no shared library (see CLAUDE.md's duplication
//! warning), so a change to one without the other silently breaks every
//! trainer generated afterward.

use anyhow::{bail, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt};
use flate2::read::DeflateDecoder;
use std::io::{Read, Seek, SeekFrom};

const MAGIC: [u8; 8] = *b"HTTOVL\0\x01";
const FOOTER_LEN: u64 = 32;

pub struct OverlayFile {
    pub filename: String,
    #[allow(dead_code)]
    pub folder: String,
    pub data: Vec<u8>,
}

/// Reads and inflates the archive appended past this process's own image.
///
/// `current_exe()` resolves to the running `.exe` under Wine exactly as it
/// does on Windows, so a trainer finds its own table with no help from the
/// launcher and no unpacking step.
///
/// `Ok(None)` means this binary simply carries no table (an ordinary
/// dev build of the runtime) - the caller falls back to `argv[1]` / a
/// `.CETRAINER` path on disk exactly as before. Anything that looks like a
/// deliberately-embedded overlay but fails to read back correctly is `Err`,
/// never `Ok(None)`: a truncated or corrupted overlay silently starting empty
/// is exactly the "trainer runs, does nothing" failure class CLAUDE.md exists
/// to prevent.
pub fn read_self_overlay() -> Result<Option<Vec<OverlayFile>>> {
    let path = std::env::current_exe().context("resolving current_exe")?;
    let mut f = std::fs::File::open(&path)
        .with_context(|| format!("opening {:?} to check for an embedded table", path))?;
    let file_len = f.metadata()?.len();
    if file_len < FOOTER_LEN {
        return Ok(None);
    }

    f.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
    let mut footer = [0u8; FOOTER_LEN as usize];
    f.read_exact(&mut footer)?;

    if footer[0..8] != MAGIC {
        return Ok(None);
    }

    let offset = u64::from_le_bytes(footer[8..16].try_into().unwrap());
    let length = u64::from_le_bytes(footer[16..24].try_into().unwrap());
    let crc = u32::from_le_bytes(footer[24..28].try_into().unwrap());

    let end = offset.checked_add(length).with_context(|| {
        format!(
            "overlay footer in {:?} has an offset/length pair that overflows",
            path
        )
    })?;
    if end > file_len - FOOTER_LEN {
        bail!(
            "overlay footer in {:?} claims a payload [{:#x}, {:#x}) past the file's own \
             length ({:#x}) - the executable is truncated or corrupted, not simply \
             missing a table",
            path,
            offset,
            end,
            file_len
        );
    }

    f.seek(SeekFrom::Start(offset))?;
    let mut payload = vec![0u8; length as usize];
    f.read_exact(&mut payload)?;

    let actual_crc = crc32fast::hash(&payload);
    if actual_crc != crc {
        bail!(
            "overlay payload in {:?} fails its CRC32 check ({:#010x} != {:#010x}) - the \
             executable is corrupted",
            path,
            actual_crc,
            crc
        );
    }

    parse_archive(&payload).map(Some)
}

/// `u32` file count, then a single
/// DEFLATE stream of `(name_len, name, folder_len, folder, data_len, data)`
/// records. Built by `ct2trainer::archive::build_archive`.
fn parse_archive(payload: &[u8]) -> Result<Vec<OverlayFile>> {
    let mut cursor = std::io::Cursor::new(payload);
    let file_count = cursor
        .read_u32::<LittleEndian>()
        .context("reading overlay archive file count")?;

    let mut decoder = DeflateDecoder::new(cursor);
    let mut out = Vec::with_capacity(file_count as usize);

    for _ in 0..file_count {
        let fn_len = decoder.read_u32::<LittleEndian>()? as usize;
        let mut fn_buf = vec![0u8; fn_len];
        decoder.read_exact(&mut fn_buf)?;
        let filename = String::from_utf8_lossy(&fn_buf).to_string();

        let folder_len = decoder.read_u32::<LittleEndian>()? as usize;
        let mut folder = String::new();
        if folder_len > 0 {
            let mut folder_buf = vec![0u8; folder_len];
            decoder.read_exact(&mut folder_buf)?;
            folder = String::from_utf8_lossy(&folder_buf).to_string();
        }

        let data_len = decoder.read_u32::<LittleEndian>()? as usize;
        let mut data = vec![0u8; data_len];
        decoder.read_exact(&mut data)?;

        out.push(OverlayFile { filename, folder, data });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use byteorder::WriteBytesExt;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn build_test_archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut inner = Vec::new();
        for (name, data) in files {
            inner.write_u32::<LittleEndian>(name.len() as u32).unwrap();
            inner.write_all(name.as_bytes()).unwrap();
            inner.write_u32::<LittleEndian>(0).unwrap(); // no folder
            inner.write_u32::<LittleEndian>(data.len() as u32).unwrap();
            inner.write_all(data).unwrap();
        }
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&inner).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut out = Vec::new();
        out.write_u32::<LittleEndian>(files.len() as u32).unwrap();
        out.extend_from_slice(&compressed);
        out
    }

    fn append_footer(mut exe: Vec<u8>, payload: &[u8]) -> Vec<u8> {
        let offset = exe.len() as u64;
        exe.extend_from_slice(payload);
        let length = payload.len() as u64;
        let crc = crc32fast::hash(payload);
        exe.extend_from_slice(&MAGIC);
        exe.extend_from_slice(&offset.to_le_bytes());
        exe.extend_from_slice(&length.to_le_bytes());
        exe.extend_from_slice(&crc.to_le_bytes());
        exe.extend_from_slice(&[0u8; 4]);
        exe
    }

    fn write_self_as(bytes: &[u8]) -> tempfile_replacement::TempExe {
        tempfile_replacement::TempExe::new(bytes)
    }

    /// A tiny stand-in for a temp-file crate: writes bytes to a scratch path
    /// and cleans up on drop. Good enough for a test that only needs a real
    /// file on disk to open and seek within.
    mod tempfile_replacement {
        pub struct TempExe {
            pub path: std::path::PathBuf,
        }
        impl TempExe {
            pub fn new(bytes: &[u8]) -> Self {
                let path = std::env::temp_dir().join(format!(
                    "htt-overlay-test-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::write(&path, bytes).unwrap();
                Self { path }
            }
        }
        impl Drop for TempExe {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    fn read_overlay_from(path: &std::path::Path) -> Result<Option<Vec<OverlayFile>>> {
        // Mirrors `read_self_overlay` but against an arbitrary path, since
        // `current_exe()` cannot be redirected from within a test.
        let mut f = std::fs::File::open(path)?;
        let file_len = f.metadata()?.len();
        if file_len < FOOTER_LEN {
            return Ok(None);
        }
        f.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
        let mut footer = [0u8; FOOTER_LEN as usize];
        f.read_exact(&mut footer)?;
        if footer[0..8] != MAGIC {
            return Ok(None);
        }
        let offset = u64::from_le_bytes(footer[8..16].try_into().unwrap());
        let length = u64::from_le_bytes(footer[16..24].try_into().unwrap());
        let crc = u32::from_le_bytes(footer[24..28].try_into().unwrap());
        let end = offset.checked_add(length).unwrap();
        if end > file_len - FOOTER_LEN {
            bail!("truncated");
        }
        f.seek(SeekFrom::Start(offset))?;
        let mut payload = vec![0u8; length as usize];
        f.read_exact(&mut payload)?;
        if crc32fast::hash(&payload) != crc {
            bail!("bad crc");
        }
        parse_archive(&payload).map(Some)
    }

    #[test]
    fn round_trips_a_two_file_archive() {
        let archive = build_test_archive(&[
            ("HTT_TRAINER.CETRAINER", b"fake cetrainer bytes"),
            ("defines.lua", b"-- defines"),
        ]);
        let exe = append_footer(b"MZfakebinarydata".to_vec(), &archive);
        let temp = write_self_as(&exe);

        let files = read_overlay_from(&temp.path).unwrap().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].filename, "HTT_TRAINER.CETRAINER");
        assert_eq!(files[0].data, b"fake cetrainer bytes");
        assert_eq!(files[1].filename, "defines.lua");
        assert_eq!(files[1].data, b"-- defines");
    }

    #[test]
    fn returns_none_for_a_plain_binary_with_no_overlay() {
        let temp = write_self_as(b"MZjust a normal build, no footer here");
        assert!(read_overlay_from(&temp.path).unwrap().is_none());
    }

    #[test]
    fn rejects_a_corrupted_payload() {
        let archive = build_test_archive(&[("x", b"y")]);
        let mut exe = append_footer(b"MZ".to_vec(), &archive);
        // Flip a byte inside the payload region without touching the footer.
        let payload_start = 4;
        exe[payload_start] ^= 0xFF;
        let temp = write_self_as(&exe);

        assert!(read_overlay_from(&temp.path).is_err());
    }

    #[test]
    fn rejects_a_truncated_file_claiming_more_than_it_has() {
        let archive = build_test_archive(&[("x", b"y")]);
        let exe = append_footer(b"MZ".to_vec(), &archive);
        // Drop bytes from inside the payload, just before the footer, so the
        // last 32 bytes (the footer, still claiming the original offset and
        // length) are untouched - simulates a partial copy or corrupted write
        // that clipped the middle of the file rather than its end.
        let footer_start = exe.len() - FOOTER_LEN as usize;
        let mut truncated = exe[..footer_start - 10].to_vec();
        truncated.extend_from_slice(&exe[footer_start..]);
        let temp = write_self_as(&truncated);

        assert!(read_overlay_from(&temp.path).is_err());
    }
}

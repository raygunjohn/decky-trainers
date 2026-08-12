use anyhow::Result;
use byteorder::{LittleEndian, WriteBytesExt};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use std::io::Write;

pub struct ArchiveFile {
    pub filename: String,
    pub folder: String,
    pub data: Vec<u8>,
}

pub fn build_archive(files: &[ArchiveFile], compression_level: Compression) -> Result<Vec<u8>> {
    let file_count = files.len() as u32;

    // Header: 4-byte uncompressed file count
    let mut header = Vec::with_capacity(4);
    header.write_u32::<LittleEndian>(file_count)?;

    // Inner stream containing file entries
    let mut inner_stream = Vec::new();

    for file in files {
        // 1. Filename length (u32 LE) + filename string bytes
        let fn_bytes = file.filename.as_bytes();
        inner_stream.write_u32::<LittleEndian>(fn_bytes.len() as u32)?;
        inner_stream.write_all(fn_bytes)?;

        // 2. Folder length (u32 LE) + folder string bytes
        let folder_bytes = file.folder.as_bytes();
        inner_stream.write_u32::<LittleEndian>(folder_bytes.len() as u32)?;
        if !folder_bytes.is_empty() {
            inner_stream.write_all(folder_bytes)?;
        }

        // 3. Data size (u32 LE) + data bytes
        inner_stream.write_u32::<LittleEndian>(file.data.len() as u32)?;
        inner_stream.write_all(&file.data)?;
    }

    // Deflate compress inner_stream
    let mut encoder = DeflateEncoder::new(Vec::new(), compression_level);
    encoder.write_all(&inner_stream)?;
    let compressed_payload = encoder.finish()?;

    let mut final_archive = Vec::with_capacity(header.len() + compressed_payload.len());
    final_archive.extend_from_slice(&header);
    final_archive.extend_from_slice(&compressed_payload);

    Ok(final_archive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use byteorder::ReadBytesExt;
    use flate2::read::DeflateDecoder;
    use std::io::Read;

    #[test]
    fn test_archive_builder() {
        let files = vec![
            ArchiveFile {
                filename: "HTT_TRAINER.CETRAINER".to_string(),
                folder: "".to_string(),
                data: b"dummy cetrainer data".to_vec(),
            },
            ArchiveFile {
                filename: "defines.lua".to_string(),
                folder: "".to_string(),
                data: b"print('defines')".to_vec(),
            },
        ];

        let archive = build_archive(&files, Compression::best()).expect("archive build failed");
        assert!(archive.len() > 4);

        let mut cursor = std::io::Cursor::new(&archive);
        let file_count = cursor.read_u32::<LittleEndian>().unwrap();
        assert_eq!(file_count, 2);

        let mut decoder = DeflateDecoder::new(&archive[4..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        assert!(!decompressed.is_empty());
    }
}

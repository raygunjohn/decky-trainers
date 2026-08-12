use anyhow::{bail, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::{Read, Write};

pub fn protect(xml_data: &[u8]) -> Result<Vec<u8>> {
    let uncompressed_size = xml_data.len() as u32;

    // Build compressed payload: 4 bytes size prefix + zlib compressed stream
    let mut compressed_payload = Vec::new();
    compressed_payload.extend_from_slice(&uncompressed_size.to_le_bytes());

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(xml_data)?;
    let zlib_bytes = encoder.finish()?;
    compressed_payload.extend_from_slice(&zlib_bytes);

    // Combine "CHEAT" header + compressed payload
    let mut buffer = Vec::with_capacity(5 + compressed_payload.len());
    buffer.extend_from_slice(b"CHEAT");
    buffer.extend_from_slice(&compressed_payload);

    // Apply 3-pass XOR obfuscation
    let len = buffer.len();
    if len > 0 {
        // Pass 1: XOR with incremental key starting at 0xCE
        let mut k: u8 = 0xCE;
        for byte in buffer.iter_mut() {
            *byte ^= k;
            k = k.wrapping_add(1);
        }

        // Pass 2: Forward XOR chain (m[i] ^= m[i+1])
        if len > 1 {
            for i in 0..len - 1 {
                buffer[i] ^= buffer[i + 1];
            }
        }

        // Pass 3: Reverse XOR chain (m[i] ^= m[i-2])
        if len > 2 {
            for i in (2..len).rev() {
                buffer[i] ^= buffer[i - 2];
            }
        }
    }

    Ok(buffer)
}

pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 9 {
        bail!("Protected trainer data too short");
    }

    let mut buffer = data.to_vec();
    let len = buffer.len();

    // Reverse Pass 3: Reverse XOR chain (m[i] ^= m[i-2])
    for i in 2..len {
        buffer[i] ^= buffer[i - 2];
    }

    // Reverse Pass 2: Forward XOR chain (m[i] ^= m[i+1])
    if len > 1 {
        for i in (0..len - 1).rev() {
            buffer[i] ^= buffer[i + 1];
        }
    }

    // Reverse Pass 1: XOR with incremental key starting at 0xCE
    let mut k: u8 = 0xCE;
    for byte in buffer.iter_mut() {
        *byte ^= k;
        k = k.wrapping_add(1);
    }

    // Check "CHEAT" header
    if &buffer[..5] != b"CHEAT" {
        bail!("Invalid protected trainer header (expected 'CHEAT')");
    }

    let payload = &buffer[5..];
    if payload.len() < 4 {
        bail!("Protected trainer payload missing size header");
    }

    let uncompressed_size = u32::from_le_bytes(payload[..4].try_into()?);
    let mut decoder = ZlibDecoder::new(&payload[4..]);

    let mut decompressed = Vec::with_capacity(uncompressed_size as usize);
    decoder.read_to_end(&mut decompressed)?;

    Ok(decompressed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protect_unprotect_roundtrip() {
        let original_xml = b"<?xml version=\"1.0\" encoding=\"utf-8\"?><CheatTable></CheatTable>";
        let protected = protect(original_xml).expect("protect failed");
        assert_ne!(protected, original_xml);

        let restored = unprotect(&protected).expect("unprotect failed");
        assert_eq!(restored, original_xml);
    }
}

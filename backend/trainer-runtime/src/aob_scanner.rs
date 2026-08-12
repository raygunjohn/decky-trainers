use crate::process::GameProcess;
use anyhow::{bail, Result};

/// Parses an AOB string into bytes and wildcards.
///
/// Two rules that are easy to get wrong and both appear in real tables:
///
/// * **The delimiters are space, comma and dash** - `delims:=[' ',',','-']` -
///   not "whitespace".
/// * **Within a word, bytes are taken two hex characters at a time**, and a
///   pair that does not parse as hex is a wildcard. So a pattern written with
///   no separators at all is legal and common:
///   `660F6E15????????4533C9` is eleven bytes, four of them wildcards.
///
/// This used to split on whitespace only, which turned a space-less pattern
/// into one enormous token and failed the scan outright. rdr2.CT's BASETIME
/// writes its patterns that way, and it is the usual style in Otis_Inf /
/// SunBeam tables.
///
/// Nibble wildcards (`7*`) collapse to a whole wildcard byte, which is the
/// behaviour whenever `canHandleNibbleWildcards` is false - the default, and
/// what every scan path here uses.
///
/// **One deliberate divergence.** The conventional parser turns *any* pair it cannot parse into a
/// wildcard (`except bytes[...]:=-1`), so `89 ZZ 41` silently scans for
/// `89 ?? 41`. Here only the wildcard characters `?` and `*` do that, and other
/// garbage is an error. A typo'd pattern that quietly matches more than the
/// author meant is the silent-success class this codebase refuses (CLAUDE.md),
/// and the cost of being strict is a loud message naming the bad token. Every
/// real pattern uses `?`/`*`, so nothing in the corpus is affected.
pub fn parse_aob_pattern(pattern_str: &str) -> Result<Vec<Option<u8>>> {
    let cleaned = pattern_str.trim().trim_matches('"');
    let mut out = Vec::new();

    for word in cleaned.split([' ', ',', '-', '\t', '\r', '\n']) {
        if word.is_empty() {
            continue;
        }
        let chars: Vec<char> = word.chars().collect();
        // The scan walks `j` by 2, taking two characters at a time, so a trailing odd
        // character forms a one-character group rather than being dropped.
        for pair in chars.chunks(2) {
            let group: String = pair.iter().collect();
            if group.contains('?') || group.contains('*') {
                out.push(None);
                continue;
            }
            match u8::from_str_radix(&group, 16) {
                Ok(b) => out.push(Some(b)),
                Err(_) => bail!(
                    "invalid byte '{}' in AOB pattern '{}' - use ? or * for a wildcard",
                    group,
                    cleaned
                ),
            }
        }
    }

    if out.is_empty() {
        bail!("empty AOB pattern");
    }
    Ok(out)
}

/// Every address matching `pattern`, capped at `limit`.
///
/// `aobscanmodule` takes the first result; a pattern with several matches
/// means the table is relying on scan order, so it is worth being able to see
/// how many there are.
pub fn aob_scan_all(
    process: &GameProcess,
    pattern: &[Option<u8>],
    module_name: Option<&str>,
    limit: usize,
) -> Result<Vec<usize>> {
    let (start, end) = scan_range(process, module_name)?;
    aob_scan_range(process, pattern, start, end, limit)
}

/// Every match inside an explicit `[start, end)`, capped at `limit`.
///
/// `aob_scan_all` bounds itself by module; `TMemScan.firstScan` takes the
/// range as arguments, and tables that scan by hand pass
/// `getAddress(process) .. getAddress(process) + getModuleSize(process)`. Same
/// walk either way - only where the bounds come from differs.
pub fn aob_scan_range(
    process: &GameProcess,
    pattern: &[Option<u8>],
    start: usize,
    end: usize,
    limit: usize,
) -> Result<Vec<usize>> {
    if pattern.is_empty() {
        bail!("empty AOB pattern");
    }
    if end <= start {
        return Ok(Vec::new());
    }
    let regions = process.readable_regions(start, end);
    let overlap = pattern.len() - 1;
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK];
    let mut hits = Vec::new();

    for (base, size) in regions {
        if size < pattern.len() {
            continue;
        }
        let mut off = 0usize;
        while off < size {
            let want = CHUNK.min(size - off);
            if want < pattern.len() {
                break;
            }
            let slice = &mut buf[..want];
            let addr = base + off;
            if process.read_memory(addr, slice).is_ok() {
                let mut from = 0usize;
                while let Some(rel) = match_pattern(&slice[from..], pattern) {
                    hits.push(addr + from + rel);
                    if hits.len() >= limit {
                        return Ok(hits);
                    }
                    from += rel + 1;
                    if from + pattern.len() > slice.len() {
                        break;
                    }
                }
            }
            if want < CHUNK {
                break;
            }
            off += CHUNK - overlap;
        }
    }
    Ok(hits)
}

fn scan_range(process: &GameProcess, module_name: Option<&str>) -> Result<(usize, usize)> {
    Ok(if let Some(m_name) = module_name {
        let key = m_name.trim().trim_matches('"').to_lowercase();
        let info = process
            .modules
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("Module '{}' not found for AOB scan", m_name))?;
        (info.base, info.base + info.size)
    } else if process.is_64bit {
        (0x10000usize, 0x7FFF_FFFF_0000usize)
    } else {
        (0x10000usize, 0x7FFF_0000usize)
    })
}

pub fn aob_scan(
    process: &GameProcess,
    pattern: &[Option<u8>],
    module_name: Option<&str>,
) -> Result<usize> {
    if pattern.is_empty() {
        bail!("Empty AOB pattern");
    }

    let (start, end) = if let Some(m_name) = module_name {
        let key = m_name.trim().trim_matches('"').to_lowercase();
        let info = process
            .modules
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("Module '{}' not found for AOB scan", m_name))?;
        (info.base, info.base + info.size)
    } else if process.is_64bit {
        (0x10000usize, 0x7FFF_FFFF_0000usize)
    } else {
        (0x10000usize, 0x7FFF_0000usize)
    };

    // Only committed, readable regions are worth reading. Scanning blindly in
    // fixed-size chunks makes a single unmapped page fail a whole 64KB read,
    // silently skipping any match inside it.
    let regions = process.readable_regions(start, end);

    const CHUNK: usize = 1 << 20;
    let overlap = pattern.len() - 1;
    let mut buf = vec![0u8; CHUNK];

    for (region_base, region_size) in regions {
        if region_size < pattern.len() {
            continue;
        }

        let mut offset = 0usize;
        while offset < region_size {
            let want = CHUNK.min(region_size - offset);
            if want < pattern.len() {
                break;
            }
            let slice = &mut buf[..want];
            let addr = region_base + offset;

            if process.read_memory(addr, slice).is_ok() {
                if let Some(found) = match_pattern(slice, pattern) {
                    return Ok(addr + found);
                }
            }

            if want < CHUNK {
                break;
            }
            // Overlap so a match spanning a chunk boundary is not missed.
            offset += CHUNK - overlap;
        }
    }

    bail!("AOB pattern match not found")
}

fn match_pattern(data: &[u8], pattern: &[Option<u8>]) -> Option<usize> {
    if pattern.len() > data.len() {
        return None;
    }

    // Anchor on the first concrete byte to skip most positions cheaply.
    let (anchor_idx, anchor_val) = pattern
        .iter()
        .enumerate()
        .find_map(|(i, p)| p.map(|b| (i, b)))
        .unwrap_or((0, 0));

    let last = data.len() - pattern.len();
    for i in 0..=last {
        if pattern[anchor_idx].is_some() && data[i + anchor_idx] != anchor_val {
            continue;
        }
        if pattern
            .iter()
            .enumerate()
            .all(|(j, p)| p.map_or(true, |b| data[i + j] == b))
        {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pattern_matching() {
        let pattern = parse_aob_pattern("89 41 ?? 5E 5D").unwrap();
        let data = vec![0x90, 0x89, 0x41, 0x10, 0x5E, 0x5D, 0xC3];
        assert_eq!(match_pattern(&data, &pattern), Some(1));
    }

    #[test]
    fn leading_wildcard_is_handled() {
        let pattern = parse_aob_pattern("?? 41 5E").unwrap();
        let data = vec![0x00, 0x99, 0x41, 0x5E];
        assert_eq!(match_pattern(&data, &pattern), Some(1));
    }

    /// `ConvertStringToBytes` splits on space, comma
    /// and dash, then eats two hex characters at a time inside each word - so a
    /// pattern written with no separators at all is legal, and is how rdr2.CT's
    /// BASETIME (and most Otis_Inf / SunBeam tables) write theirs.
    #[test]
    fn byte_pairing_rules() {
        let spaced = parse_aob_pattern("66 0F 6E 15 ?? ?? ?? ?? 45 33 C9").unwrap();
        let packed = parse_aob_pattern("660F6E15????????4533C9").unwrap();
        assert_eq!(spaced, packed, "a space-less pattern must parse identically");
        assert_eq!(packed.len(), 11);
        assert_eq!(packed[0], Some(0x66));
        assert_eq!(&packed[4..8], &[None, None, None, None]);
        assert_eq!(packed[10], Some(0xC9));

        // Commas and dashes are delimiters too, and mixing them is fine.
        assert_eq!(parse_aob_pattern("66,0F-6E 15").unwrap(),
                   vec![Some(0x66), Some(0x0F), Some(0x6E), Some(0x15)]);

        // Nibble wildcards collapse to a whole wildcard byte, as happens when
        // canHandleNibbleWildcards is false.
        assert_eq!(parse_aob_pattern("7* *7 8?").unwrap(), vec![None, None, None]);

        // A trailing odd character forms its own group rather than vanishing -
        // a two-character copy on a one-character remainder.
        assert_eq!(parse_aob_pattern("41B").unwrap(), vec![Some(0x41), Some(0x0B)]);
    }

    #[test]
    fn malformed_bytes_are_rejected_not_treated_as_wildcards() {
        assert!(parse_aob_pattern("89 ZZ 41").is_err());
        assert!(parse_aob_pattern("").is_err());
        // Real RDR2 pattern with trailing wildcards.
        let p = parse_aob_pattern("8B 55 30 48 8B CE F3 41 0F 5E 75 44 ?? ?? ?? ?? ??").unwrap();
        assert_eq!(p.len(), 17);
        assert_eq!(p[0], Some(0x8B));
        assert_eq!(p[16], None);
    }

    #[test]
    fn quoted_patterns_are_accepted() {
        let p = parse_aob_pattern("\"EB 23 F3\"").unwrap();
        assert_eq!(p, vec![Some(0xEB), Some(0x23), Some(0xF3)]);
    }
}

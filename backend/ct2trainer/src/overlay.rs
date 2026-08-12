//! Builds the trainer "overlay" format: the Windows runtime PE with a DEFLATE
//! archive and a fixed-size footer appended past its last section.
//!
//! ```text
//! ┌───────────────────────────────┬───────────────────────┬────────────────────┐
//! │ trainer-runtime.exe (verbatim)│ DEFLATE payload       │ footer (32 bytes)  │
//! │                               │  HTT_TRAINER.CETRAINER│ magic "HTTOVL\0\1" │
//! │                               │  defines.lua          │ offset u64         │
//! │                               │                       │ length u64         │
//! │                               │                       │ crc32 u32 + pad    │
//! └───────────────────────────────┴───────────────────────┴────────────────────┘
//! ```
//!
//! Appending is safe because the PE loader maps sections from the section
//! table; bytes past the last section's raw data are never mapped or
//! validated. Wine's loader behaves the same way, which is the one that
//! matters - a generated trainer only ever runs under Proton. This is the
//! technique every SFX installer and single-binary packer uses, and it works
//! identically on an ELF, which is what this format was originally written
//! for.
//!
//! **This is why `ct2trainer` needs no PE knowledge at all.** It concatenates
//! bytes. Producing a Windows trainer from a Linux host used to mean rewriting
//! the PE resource directory to plant an `RT_RCDATA` blob, and the
//! non-Windows implementation of that reported success while writing a file
//! whose resource data directory was still zero - a trainer that launched and
//! unpacked nothing. Appending has no such failure mode: either the footer is
//! there and its CRC matches, or the runtime says so and exits.
//!
//! The reader lives in `trainer-runtime/src/overlay.rs`. The two are separate
//! files in separate crates - like `ct_parser.rs`/`cetrainer.rs`, there is no
//! shared library crate (see CLAUDE.md), so the footer layout and magic must
//! be changed in both places together or the two silently diverge.

pub const MAGIC: [u8; 8] = *b"HTTOVL\0\x01";
pub const FOOTER_LEN: usize = 32;

/// `IMAGE_FILE_MACHINE_AMD64`, the only machine a generated trainer targets.
const MACHINE_AMD64: u16 = 0x8664;

/// Appends `payload` (an archive built by `archive::build_archive`) to a copy
/// of `runtime_pe`, followed by the footer the runtime reads back out of its
/// own image file.
pub fn append_overlay(runtime_pe: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(runtime_pe.len() + payload.len() + FOOTER_LEN);
    out.extend_from_slice(runtime_pe);

    let offset = out.len() as u64;
    out.extend_from_slice(payload);
    let length = payload.len() as u64;
    let crc = crc32fast::hash(payload);

    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // pad to a round 32 bytes

    out
}

/// The `Machine` field of `bytes`' COFF header, if `bytes` is a PE at all.
///
/// Walks `MZ` -> `e_lfanew` (at 0x3C) -> `PE\0\0` -> COFF header by hand
/// rather than pulling in a PE parser, because this is the only PE field the
/// generator ever needs to read. Every step is bounds-checked: `e_lfanew` is
/// attacker-adjacent in the sense that it comes from a file on disk, and a
/// panic here would be reported to the user as "the generator crashed".
fn pe_machine(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 0x40 || &bytes[..2] != b"MZ" {
        return None;
    }
    let e_lfanew = u32::from_le_bytes(bytes[0x3C..0x40].try_into().ok()?) as usize;
    // Signature (4 bytes) + Machine (2 bytes) must both be in bounds.
    let machine_end = e_lfanew.checked_add(6)?;
    if bytes.len() < machine_end || &bytes[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes(
        bytes[e_lfanew + 4..machine_end].try_into().ok()?,
    ))
}

/// Why `bytes` cannot serve as the base image for a trainer, or `None` if it
/// can.
///
/// The architecture check is not ceremony. `--runtime-dir` is a directory of build
/// artifacts and the generator runs on Linux, where the *native* build of the
/// runtime has almost the same name - so the plausible mistakes are embedding
/// a Linux ELF or a 32-bit PE, and nothing between here and the Deck would
/// catch either. It would surface as a trainer Proton refuses to start, which
/// reads as the plugin being broken rather than the build being wrong.
pub fn reject_as_runtime(bytes: &[u8]) -> Option<String> {
    match pe_machine(bytes) {
        Some(MACHINE_AMD64) => None,
        Some(other) => Some(format!(
            "it is a PE for machine {:#06x}, not x86-64 ({:#06x})",
            other, MACHINE_AMD64
        )),
        None if bytes.starts_with(b"\x7fELF") => Some(
            "it is an ELF, not a Windows PE - this is the native Linux build of the \
             runtime rather than the cross-compiled one"
                .to_string(),
        ),
        None => Some("it is not a PE executable (no MZ / PE\\0\\0 header)".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PE header just complete enough for `pe_machine`: `MZ`, an `e_lfanew`
    /// pointing past the DOS stub, `PE\0\0`, and a machine word.
    fn fake_pe(machine: u16) -> Vec<u8> {
        let mut v = vec![0u8; 0x40];
        v[0] = b'M';
        v[1] = b'Z';
        v[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        v.extend_from_slice(b"PE\0\0");
        v.extend_from_slice(&machine.to_le_bytes());
        v
    }

    #[test]
    fn footer_fields_are_at_the_documented_offsets() {
        let runtime = fake_pe(MACHINE_AMD64);
        let payload = b"fake deflate payload bytes".to_vec();

        let out = append_overlay(&runtime, &payload);

        assert_eq!(out.len(), runtime.len() + payload.len() + FOOTER_LEN);
        assert_eq!(&out[..runtime.len()], &runtime[..]);
        assert_eq!(
            &out[runtime.len()..runtime.len() + payload.len()],
            &payload[..]
        );

        let footer = &out[out.len() - FOOTER_LEN..];
        assert_eq!(&footer[0..8], &MAGIC);
        let offset = u64::from_le_bytes(footer[8..16].try_into().unwrap());
        let length = u64::from_le_bytes(footer[16..24].try_into().unwrap());
        let crc = u32::from_le_bytes(footer[24..28].try_into().unwrap());

        assert_eq!(offset, runtime.len() as u64);
        assert_eq!(length, payload.len() as u64);
        assert_eq!(crc, crc32fast::hash(&payload));
    }

    #[test]
    fn accepts_an_amd64_pe() {
        assert_eq!(reject_as_runtime(&fake_pe(MACHINE_AMD64)), None);
    }

    #[test]
    fn names_the_actual_problem_for_each_wrong_input() {
        // i386 — what a 32-bit cross-build would leave here.
        assert!(reject_as_runtime(&fake_pe(0x014c))
            .unwrap()
            .contains("not x86-64"));
        // The native Linux build of trainer-runtime, sitting in the same dir.
        assert!(reject_as_runtime(b"\x7fELF\x02\x01\x01\x00")
            .unwrap()
            .contains("ELF"));
        // Not an executable at all.
        assert!(reject_as_runtime(b"nonsense").unwrap().contains("not a PE"));
        // MZ with a garbage e_lfanew must be rejected, not panic.
        let mut corrupt = fake_pe(MACHINE_AMD64);
        corrupt[0x3C..0x40].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert!(reject_as_runtime(&corrupt).is_some());
    }
}

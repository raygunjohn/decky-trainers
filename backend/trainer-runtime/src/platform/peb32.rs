//! Module list of a 32-bit (WoW64) target, read from its 32-bit PEB.
//!
//! `CreateToolhelp32Snapshot(TH32CS_SNAPMODULE)` on a WoW64 game from this
//! 64-bit trainer fails with `ERROR_PARTIAL_COPY` (seen under Proton with
//! BioShock Remastered; documented Windows behaviour too), so the attach died
//! before anything else ran. And even where it works, the list a 32-bit table
//! needs is the 32-bit one: `call kernel32.Sleep` from injected code must
//! resolve to the 32-bit kernel32, not the 64-bit one WoW64 also maps.
//!
//! The 32-bit loader keeps exactly that list in `PEB32.Ldr`, reachable with
//! `ReadProcessMemory` alone, so the walk lives here where it needs no OS and
//! can be tested against a synthetic image.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;

use super::ModuleInfo;

/// Reads `buf.len()` bytes of the target at `addr`.
pub type Reader<'a> = &'a dyn Fn(usize, &mut [u8]) -> Result<()>;

// Field offsets in the 32-bit structures. These are the public layouts that
// have been stable since Windows XP; CE and every injector rely on them.
/// `PEB32.Ldr`
const PEB32_LDR: usize = 0x0C;
/// `PEB_LDR_DATA32.InLoadOrderModuleList`
const LDR32_IN_LOAD_ORDER_MODULE_LIST: usize = 0x0C;
/// `LDR_DATA_TABLE_ENTRY32.DllBase` (`InLoadOrderLinks` is at 0, so a link
/// address is the entry address)
const ENTRY32_DLL_BASE: usize = 0x18;
/// `LDR_DATA_TABLE_ENTRY32.SizeOfImage`
const ENTRY32_SIZE_OF_IMAGE: usize = 0x20;
/// `LDR_DATA_TABLE_ENTRY32.BaseDllName`, a `UNICODE_STRING32`:
/// `Length: u16, MaximumLength: u16, Buffer: u32`
const ENTRY32_BASE_DLL_NAME: usize = 0x2C;

/// A loader list longer than this is a cycle through corrupt memory, not a
/// process with that many DLLs.
const MAX_MODULES: usize = 4096;

/// Walks `InLoadOrderModuleList` of the 32-bit PEB at `peb32`.
///
/// Names are lower-cased, as `enumerate_modules` promises its callers.
pub fn modules_from_peb32(read: Reader, peb32: usize) -> Result<HashMap<String, ModuleInfo>> {
    let u32_at = |addr: usize| -> Result<u32> {
        let mut b = [0u8; 4];
        read(addr, &mut b).with_context(|| format!("reading the 32-bit loader list at {:#X}", addr))?;
        Ok(u32::from_le_bytes(b))
    };
    let u16_at = |addr: usize| -> Result<u16> {
        let mut b = [0u8; 2];
        read(addr, &mut b).with_context(|| format!("reading the 32-bit loader list at {:#X}", addr))?;
        Ok(u16::from_le_bytes(b))
    };

    let ldr = u32_at(peb32 + PEB32_LDR)? as usize;
    if ldr == 0 {
        bail!("PEB32.Ldr is null: the 32-bit loader has not initialised yet");
    }

    let head = ldr + LDR32_IN_LOAD_ORDER_MODULE_LIST;
    let mut cur = u32_at(head)? as usize;
    let mut out = HashMap::new();
    let mut seen = 0usize;

    while cur != head && cur != 0 {
        seen += 1;
        if seen > MAX_MODULES {
            bail!("the 32-bit loader list does not terminate");
        }

        let base = u32_at(cur + ENTRY32_DLL_BASE)? as usize;
        let size = u32_at(cur + ENTRY32_SIZE_OF_IMAGE)? as usize;
        let name_len = u16_at(cur + ENTRY32_BASE_DLL_NAME)? as usize;
        let name_ptr = u32_at(cur + ENTRY32_BASE_DLL_NAME + 4)? as usize;

        if base != 0 && name_ptr != 0 && name_len >= 2 {
            let mut buf = vec![0u8; name_len.min(512) & !1];
            read(name_ptr, &mut buf)
                .with_context(|| format!("reading a module name at {:#X}", name_ptr))?;
            let units: Vec<u16> =
                buf.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let name = String::from_utf16_lossy(&units).to_lowercase();
            out.insert(name.clone(), ModuleInfo { name, base, size });
        }

        cur = u32_at(cur)? as usize; // InLoadOrderLinks.Flink
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake 32-bit address space: one byte vector mapped at `base`.
    struct Image {
        base: usize,
        bytes: Vec<u8>,
    }

    impl Image {
        fn new(base: usize, size: usize) -> Self {
            Image { base, bytes: vec![0; size] }
        }
        fn put32(&mut self, addr: usize, v: u32) {
            let o = addr - self.base;
            self.bytes[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
        fn put16(&mut self, addr: usize, v: u16) {
            let o = addr - self.base;
            self.bytes[o..o + 2].copy_from_slice(&v.to_le_bytes());
        }
        fn put_utf16(&mut self, addr: usize, s: &str) -> u16 {
            let mut o = addr - self.base;
            for u in s.encode_utf16() {
                self.bytes[o..o + 2].copy_from_slice(&u.to_le_bytes());
                o += 2;
            }
            (s.encode_utf16().count() * 2) as u16
        }
        fn reader(&self) -> impl Fn(usize, &mut [u8]) -> Result<()> + '_ {
            move |addr, buf| {
                let o = addr.checked_sub(self.base).ok_or_else(|| anyhow::anyhow!("unmapped"))?;
                let end = o + buf.len();
                if end > self.bytes.len() {
                    bail!("unmapped read at {:#X}", addr);
                }
                buf.copy_from_slice(&self.bytes[o..end]);
                Ok(())
            }
        }
    }

    /// Lays out a PEB32, its PEB_LDR_DATA32 and `entries` loader entries as a
    /// circular InLoadOrder list, the way the 32-bit ntdll does.
    fn build(entries: &[(&str, u32, u32)]) -> (Image, usize) {
        let base = 0x0030_0000usize;
        let mut img = Image::new(base, 0x4000);
        let peb = base;
        let ldr = base + 0x100;
        let head = ldr + LDR32_IN_LOAD_ORDER_MODULE_LIST;
        img.put32(peb + PEB32_LDR, ldr as u32);

        let entry_at = |i: usize| base + 0x1000 + i * 0x80;
        let name_at = |i: usize| base + 0x3000 + i * 0x40;
        let n = entries.len();

        // Head links to first entry, last entry links back to head.
        img.put32(head, if n > 0 { entry_at(0) as u32 } else { head as u32 });
        for (i, (name, dll_base, size)) in entries.iter().enumerate() {
            let e = entry_at(i);
            let next = if i + 1 < n { entry_at(i + 1) } else { head };
            img.put32(e, next as u32); // Flink
            img.put32(e + ENTRY32_DLL_BASE, *dll_base);
            img.put32(e + ENTRY32_SIZE_OF_IMAGE, *size);
            let len = img.put_utf16(name_at(i), name);
            img.put16(e + ENTRY32_BASE_DLL_NAME, len);
            img.put32(e + ENTRY32_BASE_DLL_NAME + 4, name_at(i) as u32);
        }
        (img, peb)
    }

    #[test]
    fn walks_the_in_load_order_list_and_lowercases_names() {
        let (img, peb) = build(&[
            ("BioshockHD.exe", 0x0040_0000, 0x0120_0000),
            ("ntdll.dll", 0x7BC0_0000, 0x0017_0000),
            ("KERNEL32.DLL", 0x7B40_0000, 0x0012_0000),
        ]);
        let mods = modules_from_peb32(&img.reader(), peb).unwrap();
        assert_eq!(mods.len(), 3);
        let game = &mods["bioshockhd.exe"];
        assert_eq!((game.base, game.size), (0x0040_0000, 0x0120_0000));
        // The key a table's `kernel32.Sleep` lookup uses.
        assert_eq!(mods["kernel32.dll"].base, 0x7B40_0000);
    }

    #[test]
    fn an_empty_list_is_empty_not_an_error() {
        let (img, peb) = build(&[]);
        assert!(modules_from_peb32(&img.reader(), peb).unwrap().is_empty());
    }

    #[test]
    fn a_null_ldr_is_reported() {
        let img = Image::new(0x0030_0000, 0x100);
        let err = modules_from_peb32(&img.reader(), 0x0030_0000).unwrap_err().to_string();
        assert!(err.contains("Ldr is null"), "{}", err);
    }

    /// A Flink pointing at itself must not spin forever.
    #[test]
    fn a_cycle_that_never_returns_to_the_head_is_rejected() {
        let (mut img, peb) = build(&[("a.dll", 0x1000_0000, 0x1000)]);
        let e = 0x0030_0000 + 0x1000;
        img.put32(e, e as u32);
        let err = modules_from_peb32(&img.reader(), peb).unwrap_err().to_string();
        assert!(err.contains("does not terminate"), "{}", err);
    }

    /// An unreadable entry names the address, since that is what tells a
    /// Proton bug from a table bug.
    #[test]
    fn an_unreadable_entry_names_the_address() {
        let (mut img, peb) = build(&[("a.dll", 0x1000_0000, 0x1000)]);
        img.put32(0x0030_0000 + 0x100 + LDR32_IN_LOAD_ORDER_MODULE_LIST, 0xDEAD_0000);
        let err = format!("{:#}", modules_from_peb32(&img.reader(), peb).unwrap_err());
        assert!(err.contains("0xDEAD0018"), "{}", err);
    }
}

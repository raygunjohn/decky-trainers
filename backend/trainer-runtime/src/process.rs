//! The attached target process.
//!
//! Portable by construction: every OS-specific operation goes through
//! `platform::ProcessBackend`. What lives here is the logic layered on those
//! primitives — the near-allocation sweep, region enumeration, module lookup
//! and PE export walking — none of which needs to be reimplemented per
//! platform.

use anyhow::Result;
use std::collections::HashMap;

use crate::platform::{self, ProcessBackend};

pub use crate::platform::ModuleInfo;

pub struct GameProcess {
    pub pid: u32,
    pub name: String,
    pub is_64bit: bool,
    pub modules: HashMap<String, ModuleInfo>,
    backend: Box<dyn ProcessBackend>,
}

impl GameProcess {
    pub fn attach_by_name(process_name: &str) -> Result<Self> {
        let attached = platform::attach_by_name(process_name)?;

        let mut proc = GameProcess {
            pid: attached.pid,
            name: process_name.to_string(),
            is_64bit: attached.is_64bit,
            modules: HashMap::new(),
            backend: attached.backend,
        };

        proc.refresh_modules()?;
        Ok(proc)
    }

    pub fn refresh_modules(&mut self) -> Result<()> {
        self.modules = self.backend.enumerate_modules()?;
        Ok(())
    }

    pub fn read_memory(&self, addr: usize, buf: &mut [u8]) -> Result<()> {
        self.backend.read(addr, buf)
    }

    pub fn write_memory(&self, addr: usize, data: &[u8]) -> Result<()> {
        self.backend.write(addr, data)
    }

    /// Allocates memory as close to `preferred` as possible.
    ///
    /// Injected code is reached with a 5-byte `jmp rel32`, so the trampoline has
    /// to land within ±2GB of the hook site. A plain allocation with no preferred
    /// address can easily land further away in a 64-bit address space, which
    /// makes the hook unencodable. Mirrors the `lastChanceAllocPrefered`.
    ///
    /// The sweep depends on `alloc_at` refusing an occupied address rather than
    /// relocating — see the contract on `ProcessBackend::alloc_at`.
    pub fn alloc_memory_near(&self, preferred: usize, size: usize) -> Result<usize> {
        if preferred == 0 {
            return self.alloc_memory(size);
        }

        const GRANULARITY: usize = 0x10000;
        const MAX_DISTANCE: usize = 0x7000_0000; // stay well inside ±2GB

        let base = preferred - (preferred % GRANULARITY);

        let mut distance = 0usize;
        while distance < MAX_DISTANCE {
            for candidate in [base.checked_add(distance), base.checked_sub(distance)]
                .into_iter()
                .flatten()
            {
                if candidate == 0 {
                    continue;
                }
                if let Ok(addr) = self.backend.alloc_at(Some(candidate), size) {
                    return Ok(addr);
                }
            }
            distance += GRANULARITY;
        }

        // Nothing free nearby; fall back and let the assembler report if a
        // branch turns out to be unencodable.
        self.alloc_memory(size)
    }

    /// Enumerates committed, readable memory regions in `[start, end)`.
    pub fn readable_regions(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut addr = start;

        while addr < end {
            let Some(region) = self.backend.query_region(addr) else {
                break;
            };
            if region.size == 0 {
                break;
            }

            if region.readable {
                let s = region.base.max(start);
                let e = (region.base + region.size).min(end);
                if s < e {
                    out.push((s, e - s));
                }
            }

            let next = region.base.saturating_add(region.size);
            if next <= region.base {
                break;
            }
            addr = next;
        }

        out
    }

    pub fn alloc_memory(&self, size: usize) -> Result<usize> {
        self.backend.alloc_at(None, size)
    }

    pub fn free_memory(&self, addr: usize) -> Result<()> {
        self.backend.free(addr)
    }

    /// True while the target process is still running.
    ///
    /// An open handle keeps the kernel object alive after the process exits, so
    /// the handle stays "valid" and attachment silently looks healthy while
    /// every read fails. The backend probes the process itself.
    pub fn is_alive(&self) -> bool {
        self.backend.is_alive()
    }

    /// Base of the process's own executable.
    ///
    /// Used as the default anchor for `alloc()` without a preferred address:
    /// injected code reaches its data with RIP-relative displacements, so the
    /// allocation has to land within ±2GB of the module.
    pub fn main_module_base(&self) -> usize {
        self.get_module_base(&self.name)
            .or_else(|| self.modules.values().map(|m| m.base).min())
            .unwrap_or(0)
    }

    pub fn get_module_base(&self, mod_name: &str) -> Option<usize> {
        let key = mod_name.to_lowercase();
        self.modules.get(&key).map(|m| m.base)
    }

    /// Starts a thread in the target process at `addr`.
    ///
    /// the `createthread(x)` directive. Tables use it for polling workers, such
    /// as the waypoint-teleport loop that watches a flag and moves the player.
    ///
    /// Resolves `kernel32.dll!CreateThread` first and hands it to the backend:
    /// on a Wine-hosted target the backend uses it to make the *target* spawn
    /// its own thread, so Wine's real thread-creation path registers it (TEB,
    /// wineserver) the same as it would for any other Windows program. See
    /// `ProcessBackend::create_thread`.
    pub fn create_thread(&self, addr: usize) -> Result<u32> {
        let remote_create_thread = self.get_export("kernel32.dll", "CreateThread");
        self.backend.create_thread(addr, remote_create_thread)
    }

    /// Resolves an exported function, e.g. `kernel32.dll` + `Sleep`.
    ///
    /// Auto Assembler scripts call OS functions by name (`call kernel32.Sleep`),
    /// so the export directory of the loaded module has to be walked in the
    /// target process. Comparison is case-insensitive, as tables expect.
    ///
    /// Platform-independent despite parsing PE structures: it only needs
    /// `read_memory`, and Wine maps genuine PE images, so this works unchanged
    /// against a target running under Proton.
    pub fn get_export(&self, module: &str, export: &str) -> Option<usize> {
        let key = module.to_lowercase();
        let info = self
            .modules
            .get(&key)
            .or_else(|| self.modules.get(&format!("{}.dll", key)))?;
        let base = info.base;

        let u32_at = |addr: usize| -> Option<u32> {
            let mut b = [0u8; 4];
            self.read_memory(addr, &mut b).ok()?;
            Some(u32::from_le_bytes(b))
        };

        // DOS header -> PE header
        let e_lfanew = u32_at(base + 0x3C)? as usize;
        let pe = base + e_lfanew;
        let mut sig = [0u8; 4];
        self.read_memory(pe, &mut sig).ok()?;
        if &sig != b"PE\0\0" {
            return None;
        }

        // Optional header magic tells us 32- vs 64-bit; the data directory
        // sits at a different offset in each.
        let opt = pe + 24;
        let mut magic = [0u8; 2];
        self.read_memory(opt, &mut magic).ok()?;
        let dd = match u16::from_le_bytes(magic) {
            0x20B => opt + 112, // PE32+
            0x10B => opt + 96,  // PE32
            _ => return None,
        };

        let export_rva = u32_at(dd)? as usize;
        let export_size = u32_at(dd + 4)? as usize;
        if export_rva == 0 {
            return None;
        }
        let exp = base + export_rva;

        let ordinal_base = u32_at(exp + 16)?;
        let n_names = u32_at(exp + 24)? as usize;
        let funcs = base + u32_at(exp + 28)? as usize;
        let names = base + u32_at(exp + 32)? as usize;
        let ordinals = base + u32_at(exp + 36)? as usize;

        let want = export.to_lowercase();
        let mut name_buf = [0u8; 128];

        for i in 0..n_names {
            let name_rva = u32_at(names + i * 4)? as usize;
            if self.read_memory(base + name_rva, &mut name_buf).is_err() {
                continue;
            }
            let end = name_buf.iter().position(|b| *b == 0).unwrap_or(name_buf.len());
            let name = String::from_utf8_lossy(&name_buf[..end]).to_lowercase();
            if name != want {
                continue;
            }

            let mut ob = [0u8; 2];
            self.read_memory(ordinals + i * 2, &mut ob).ok()?;
            let idx = u16::from_le_bytes(ob) as usize;
            let func_rva = u32_at(funcs + idx * 4)? as usize;

            // An RVA inside the export directory is a forwarder string
            // ("NTDLL.RtlEnterCriticalSection"), not code.
            if func_rva >= export_rva && func_rva < export_rva + export_size {
                let mut fb = [0u8; 128];
                self.read_memory(base + func_rva, &mut fb).ok()?;
                let e = fb.iter().position(|b| *b == 0).unwrap_or(fb.len());
                let fwd = String::from_utf8_lossy(&fb[..e]).to_string();
                let (m, f) = fwd.split_once('.')?;
                if m.eq_ignore_ascii_case(module) && f.eq_ignore_ascii_case(export) {
                    return None; // self-referential; give up rather than loop
                }
                return self.get_export(m, f);
            }

            let _ = ordinal_base;
            return Some(base + func_rva);
        }
        None
    }
}

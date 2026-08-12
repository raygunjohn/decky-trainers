//! The platform seam.
//!
//! Everything above this module — the assembler, the AA engine, symbol
//! resolution, AOB scanning, Lua, the REST server — is portable and must stay
//! that way. Every OS-specific operation the engine performs on a *foreign*
//! process passes through `ProcessBackend`.
//!
//! The trait is deliberately narrow. Logic that can be expressed on top of the
//! primitives lives in `process.rs` instead, so a new backend owes eight
//! methods rather than the fifteen `GameProcess` exposes. Two cases are worth
//! calling out because they look like they belong here and do not:
//!
//! * **Making a page writable.** A hook write needs `VirtualProtectEx` around
//!   it. That is an implementation detail of "write these bytes reliably", so
//!   it lives inside `write` and never appears on the seam.
//! * **Walking a PE export directory.** `call kernel32.Sleep` needs the export
//!   table of the loaded module, but that is just parsing bytes obtained via
//!   `read`, so it lives in `process.rs`.
//!
//! **There is one real backend: `win32`.** A trainer is a Windows PE that runs
//! inside the game's Proton prefix, which makes the game an ordinary process
//! to it and `ReadProcessMemory`/`VirtualAllocEx`/`CreateRemoteThread` the
//! same primitives such tools use. A native-Linux ptrace backend existed
//! and was deleted: it worked, but every allocation was a `PTRACE_ATTACH` —
//! i.e. a SIGSTOP of a game thread — and `createthread` could never find a
//! thread of RDR2 it was allowed to hijack. Both problems are absent here
//! because neither API stops anything. See CLAUDE.md.
//!
//! Backends must never report success for something they did not do. A backend
//! that cannot allocate returns an error; it does not return a plausible
//! address. See the "never skip a construct you do not understand" rule in
//! CLAUDE.md — a silent success here surfaces as a table that looks broken.

use anyhow::Result;
use std::collections::HashMap;

#[cfg(windows)]
mod win32;

// Linux and macOS dev hosts: the engine still parses tables, assembles, and
// runs the offline analysis modes; it just cannot attach.
#[cfg(not(windows))]
mod unsupported;

#[derive(Debug, Clone)]
pub struct ModuleInfo {
    pub name: String,
    pub base: usize,
    pub size: usize,
}

/// One contiguous mapping in the target's address space.
///
/// Reported from `VirtualQueryEx`. `readable` is resolved by the backend
/// rather than by the caller, because "unreadable" is not one concept: Windows
/// has the `MEM_RESERVE`/`MEM_COMMIT` split plus `PAGE_GUARD` and
/// `PAGE_NOACCESS`, and a backend on a system with neither reservation nor
/// guard pages would have to fold its own model into this one flag.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub base: usize,
    pub size: usize,
    pub readable: bool,
}

/// A live attachment to a foreign process.
pub trait ProcessBackend: Send + Sync {
    /// Reads exactly `buf.len()` bytes, or fails. A short read is an error, not
    /// a partial success — callers treat the buffer as fully populated.
    fn read(&self, addr: usize, buf: &mut [u8]) -> Result<()>;

    /// Writes all of `data`, making the target page writable if that is what
    /// the platform requires, and restoring the original protection after.
    fn write(&self, addr: usize, data: &[u8]) -> Result<()>;

    /// Reserves and commits `size` bytes of RWX memory.
    ///
    /// With `Some(addr)` the allocation must land exactly there or fail — the
    /// caller sweeps outward from a hook site looking for a free slot, so a
    /// backend that silently relocates breaks the ±2GB guarantee that makes
    /// `jmp rel32` encodable. `VirtualAllocEx` behaves this way natively.
    fn alloc_at(&self, preferred: Option<usize>, size: usize) -> Result<usize>;

    fn free(&self, addr: usize) -> Result<()>;

    /// Describes the mapping containing `addr`. `None` ends region enumeration.
    ///
    /// Two contracts the region walk depends on:
    ///
    /// * The returned region must end **strictly above** `addr`
    ///   (`base + size > addr`). The caller advances by stepping to the end of
    ///   each region; a region ending at or below `addr` loops forever.
    /// * Unmapped space must be reported as an unreadable region spanning the
    ///   gap, not as `None`. `VirtualQueryEx` does this natively via `MEM_FREE`.
    ///   Returning `None` at the first gap silently truncates every AOB scan to
    ///   the lowest contiguous run.
    fn query_region(&self, addr: usize) -> Option<Region>;

    /// Starts a thread in the target at `addr`, returning its thread id.
    /// Backs the `createthread` directive.
    ///
    /// `remote_create_thread`, when present, is the target's own resolved
    /// `kernel32.dll!CreateThread` (see `GameProcess::get_export`). The
    /// `win32` backend ignores it: `CreateRemoteThread` goes through the same
    /// machinery, so the new thread is registered with wineserver — TEB, `FS`
    /// base and all — from the start. The parameter survives because
    /// `process.rs` resolves the export anyway and a backend that needs to
    /// call it itself is a real possibility.
    fn create_thread(&self, addr: usize, remote_create_thread: Option<usize>) -> Result<u32>;

    /// False once the target has exited.
    ///
    /// Must genuinely probe the process. A Windows handle stays valid after the
    /// target dies, so attachment looks healthy while every read fails; the
    /// freeze loop relies on this to detach and reset records.
    fn is_alive(&self) -> bool;

    fn enumerate_modules(&self) -> Result<HashMap<String, ModuleInfo>>;
}

/// What `attach_by_name` hands back before `GameProcess` wraps it.
pub struct Attached {
    pub backend: Box<dyn ProcessBackend>,
    pub pid: u32,
    /// False for a 32-bit target, which changes pointer width in the address
    /// parser and the assembler's operand defaults.
    pub is_64bit: bool,
}

pub fn attach_by_name(process_name: &str) -> Result<Attached> {
    #[cfg(windows)]
    {
        win32::attach_by_name(process_name)
    }
    #[cfg(not(windows))]
    {
        unsupported::attach_by_name(process_name)
    }
}

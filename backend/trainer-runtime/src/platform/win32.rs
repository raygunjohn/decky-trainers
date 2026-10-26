//! Windows `ProcessBackend`, on the Win32 process and memory APIs.

use anyhow::{bail, Result};
use std::collections::HashMap;

use windows::{
    Win32::Foundation::{CloseHandle, HANDLE},
    Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory},
    Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32First, Module32Next, Process32First, Process32Next,
        MODULEENTRY32, PROCESSENTRY32, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
    },
    Win32::System::Memory::{
        VirtualAllocEx, VirtualFreeEx, VirtualProtectEx, VirtualQueryEx, MEM_COMMIT, MEM_FREE,
        MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE, PAGE_GUARD,
        PAGE_NOACCESS, PAGE_PROTECTION_FLAGS,
    },
    Win32::System::Threading::{
        GetExitCodeProcess, IsWow64Process, OpenProcess, PROCESS_ALL_ACCESS,
    },
};

use super::{Attached, ModuleInfo, ProcessBackend, Region};

pub struct WindowsBackend {
    pid: u32,
    handle: HANDLE,
    /// A 32-bit target. Its module list has to come from its 32-bit PEB:
    /// see `enumerate_modules`.
    is_wow64: bool,
}

// `ProcessWow64Information`: the 32-bit PEB address of a WoW64 process,
// zero for a native one. Declared here rather than through the `windows`
// crate's `Wdk` feature set, since this is the only NT API the backend uses.
#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        info: *mut std::ffi::c_void,
        len: u32,
        return_len: *mut u32,
    ) -> i32;
}
const PROCESS_WOW64_INFORMATION: u32 = 26;

// HANDLE is a raw pointer, so it is neither Send nor Sync by inference. The
// handle is only ever passed to Win32 calls that are themselves thread-safe,
// and it is never dereferenced here. Asserting it on this type rather than on
// `GameProcess` keeps the unsafe claim next to the field it is about.
unsafe impl Send for WindowsBackend {}
unsafe impl Sync for WindowsBackend {}

pub fn attach_by_name(process_name: &str) -> Result<Attached> {
    unsafe {
        let pid = find_process_id_by_name(process_name)?;

        // A failure here is almost always ERROR_ACCESS_DENIED (5) because
        // the trainer is not elevated. Say so instead of a bare error code:
        // this silently produced "waiting forever" before.
        let handle = match OpenProcess(PROCESS_ALL_ACCESS, false, pid) {
            Ok(h) => h,
            Err(e) => {
                if e.code().0 as u32 & 0xFFFF == 5 {
                    bail!(
                        "Access denied opening {} (PID {}). Run the trainer as \
                         Administrator - the game runs at a privilege level this \
                         process cannot touch.",
                        process_name,
                        pid
                    );
                }
                bail!("Failed to open {} (PID {}): {}", process_name, pid, e);
            }
        };

        if handle.is_invalid() {
            bail!("Failed to open process {} (PID {})", process_name, pid);
        }

        let mut is_wow64 = false;
        let _ = IsWow64Process(handle, &mut is_wow64 as *mut bool as *mut _);

        Ok(Attached {
            backend: Box::new(WindowsBackend { pid, handle, is_wow64 }),
            pid,
            is_64bit: !is_wow64,
        })
    }
}

impl ProcessBackend for WindowsBackend {
    fn read(&self, addr: usize, buf: &mut [u8]) -> Result<()> {
        unsafe {
            let mut read_bytes = 0usize;
            let res = ReadProcessMemory(
                self.handle,
                addr as *const _,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                Some(&mut read_bytes),
            );

            if res.is_ok() && read_bytes == buf.len() {
                Ok(())
            } else {
                bail!("ReadProcessMemory failed at address {:#X}", addr)
            }
        }
    }

    fn write(&self, addr: usize, data: &[u8]) -> Result<()> {
        unsafe {
            if data.is_empty() {
                return Ok(());
            }

            let mut written_bytes = 0usize;
            let mut res = WriteProcessMemory(
                self.handle,
                addr as *mut _,
                data.as_ptr() as *const _,
                data.len(),
                Some(&mut written_bytes),
            );

            // Code pages are typically PAGE_EXECUTE_READ; temporarily make them
            // writable so hooks can be installed into the target's .text section.
            if res.is_err() || written_bytes != data.len() {
                let mut old = PAGE_PROTECTION_FLAGS(0);
                let protect_ok = VirtualProtectEx(
                    self.handle,
                    addr as *mut _,
                    data.len(),
                    PAGE_EXECUTE_READWRITE,
                    &mut old,
                )
                .is_ok();

                if protect_ok {
                    written_bytes = 0;
                    res = WriteProcessMemory(
                        self.handle,
                        addr as *mut _,
                        data.as_ptr() as *const _,
                        data.len(),
                        Some(&mut written_bytes),
                    );

                    let mut restored = PAGE_PROTECTION_FLAGS(0);
                    let _ = VirtualProtectEx(
                        self.handle,
                        addr as *mut _,
                        data.len(),
                        old,
                        &mut restored,
                    );
                }
            }

            if res.is_ok() && written_bytes == data.len() {
                Ok(())
            } else {
                bail!("WriteProcessMemory failed at address {:#X}", addr)
            }
        }
    }

    fn alloc_at(&self, preferred: Option<usize>, size: usize) -> Result<usize> {
        unsafe {
            let ptr = VirtualAllocEx(
                self.handle,
                preferred.map(|a| a as *const _),
                size,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            );

            if ptr.is_null() {
                match preferred {
                    Some(a) => bail!("VirtualAllocEx failed to allocate {} bytes at {:#X}", size, a),
                    None => bail!("VirtualAllocEx failed to allocate {} bytes", size),
                }
            }

            Ok(ptr as usize)
        }
    }

    fn free(&self, addr: usize) -> Result<()> {
        unsafe {
            if VirtualFreeEx(self.handle, addr as *mut _, 0, MEM_RELEASE).is_ok() {
                Ok(())
            } else {
                bail!("VirtualFreeEx failed at address {:#X}", addr)
            }
        }
    }

    fn query_region(&self, addr: usize) -> Option<Region> {
        unsafe {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            let written = VirtualQueryEx(
                self.handle,
                Some(addr as *const _),
                &mut mbi,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            );
            if written == 0 {
                return None;
            }

            let committed = mbi.State != MEM_FREE && mbi.State.0 == MEM_COMMIT.0;
            let prot = mbi.Protect.0;

            Some(Region {
                base: mbi.BaseAddress as usize,
                size: mbi.RegionSize,
                readable: committed
                    && prot & PAGE_NOACCESS.0 == 0
                    && prot & PAGE_GUARD.0 == 0,
            })
        }
    }

    fn create_thread(&self, addr: usize, _remote_create_thread: Option<usize>) -> Result<u32> {
        // `CreateRemoteThread` already does everything `remote_create_thread`
        // exists to work around on Linux: it's the real OS API, so the new
        // thread is properly registered from the start.
        unsafe {
            use windows::Win32::System::Threading::{CreateRemoteThread, LPTHREAD_START_ROUTINE};

            let start: LPTHREAD_START_ROUTINE = Some(std::mem::transmute::<
                usize,
                unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
            >(addr));

            let mut tid: u32 = 0;
            let handle =
                CreateRemoteThread(self.handle, None, 0, start, None, 0, Some(&mut tid))?;

            if handle.is_invalid() {
                bail!("CreateRemoteThread failed at {:#X}", addr);
            }
            let _ = CloseHandle(handle);
            Ok(tid)
        }
    }

    fn is_alive(&self) -> bool {
        unsafe {
            const STILL_ACTIVE: u32 = 259;
            let mut code: u32 = 0;
            if GetExitCodeProcess(self.handle, &mut code).is_err() {
                return false;
            }
            code == STILL_ACTIVE
        }
    }

    fn enumerate_modules(&self) -> Result<HashMap<String, ModuleInfo>> {
        // Toolhelp cannot snapshot a WoW64 target's modules from a 64-bit
        // caller: it fails with ERROR_PARTIAL_COPY, which is what used to
        // kill every attach to a 32-bit game. The 32-bit PEB has the list
        // the table wants anyway (the 32-bit kernel32, not the 64-bit one).
        if self.is_wow64 {
            match self.modules_via_peb32() {
                Ok(m) if !m.is_empty() => return Ok(m),
                Ok(_) => tracing::warn!("the 32-bit loader list is empty; trying Toolhelp"),
                Err(e) => tracing::warn!("32-bit loader walk failed ({:#}); trying Toolhelp", e),
            }
        }
        unsafe { self.modules_via_toolhelp() }
    }
}

impl WindowsBackend {
    fn modules_via_peb32(&self) -> Result<HashMap<String, ModuleInfo>> {
        let mut peb32: usize = 0;
        let mut len = 0u32;
        let status = unsafe {
            NtQueryInformationProcess(
                self.handle,
                PROCESS_WOW64_INFORMATION,
                &mut peb32 as *mut usize as *mut std::ffi::c_void,
                std::mem::size_of::<usize>() as u32,
                &mut len,
            )
        };
        if status < 0 {
            bail!("NtQueryInformationProcess(ProcessWow64Information) failed: NTSTATUS {:#X}", status);
        }
        if peb32 == 0 {
            bail!("the target reports no 32-bit PEB, so it is not a WoW64 process");
        }
        let read = |addr: usize, buf: &mut [u8]| self.read(addr, buf);
        super::peb32::modules_from_peb32(&read, peb32)
    }

    unsafe fn modules_via_toolhelp(&self) -> Result<HashMap<String, ModuleInfo>> {
        {
            let mut modules = HashMap::new();
            let snapshot =
                CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, self.pid)?;

            if snapshot.is_invalid() {
                bail!("Failed to create module snapshot for PID {}", self.pid);
            }

            let mut entry = MODULEENTRY32 {
                dwSize: std::mem::size_of::<MODULEENTRY32>() as u32,
                ..Default::default()
            };

            if Module32First(snapshot, &mut entry).is_ok() {
                loop {
                    let mod_name = String::from_utf8_lossy(
                        &entry.szModule[..entry
                            .szModule
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(entry.szModule.len())]
                            .iter()
                            .map(|&c| c as u8)
                            .collect::<Vec<u8>>(),
                    )
                    .to_lowercase();

                    modules.insert(
                        mod_name.clone(),
                        ModuleInfo {
                            name: mod_name,
                            base: entry.modBaseAddr as usize,
                            size: entry.modBaseSize as usize,
                        },
                    );

                    if Module32Next(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }

            let _ = CloseHandle(snapshot);
            Ok(modules)
        }
    }
}

impl Drop for WindowsBackend {
    fn drop(&mut self) {
        unsafe {
            if !self.handle.is_invalid() {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

unsafe fn find_process_id_by_name(target_name: &str) -> Result<u32> {
    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
    if snapshot.is_invalid() {
        bail!("Failed to create process snapshot");
    }

    let mut entry = PROCESSENTRY32 {
        dwSize: std::mem::size_of::<PROCESSENTRY32>() as u32,
        ..Default::default()
    };

    let target_lower = target_name.to_lowercase();

    if Process32First(snapshot, &mut entry).is_ok() {
        loop {
            let exe_name = String::from_utf8_lossy(
                &entry.szExeFile[..entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len())]
                    .iter()
                    .map(|&c| c as u8)
                    .collect::<Vec<u8>>(),
            )
            .to_lowercase();

            if exe_name == target_lower || exe_name == format!("{}.exe", target_lower) {
                let _ = CloseHandle(snapshot);
                return Ok(entry.th32ProcessID);
            }

            if Process32Next(snapshot, &mut entry).is_err() {
                break;
            }
        }
    }

    let _ = CloseHandle(snapshot);
    bail!("Process '{}' not found", target_name)
}

//! Reading and writing game memory.
//!
//! Data is read and written with `ReadProcessMemory` / `WriteProcessMemory`,
//! so a stale pointer fails cleanly instead of crashing. Code is written with
//! one aligned store (`write_code_u16`, `write_code_u32`), so a game thread
//! running that code sees either the old or the new instruction.

use std::ffi::c_void;

use windows::Win32::System::Diagnostics::Debug::{
    FlushInstructionCache, ReadProcessMemory, WriteProcessMemory,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, VirtualProtect,
};
use windows::Win32::System::Threading::GetCurrentProcess;

/// Read access to memory, so code that walks game structures can be tested
/// against a fake memory.
pub trait Memory {
    /// Reads a value; `None` if the address is not readable.
    fn read<T: Copy + Default>(&self, addr: usize) -> Option<T>;

    /// Reads a pointer; 0 if unreadable.
    fn read_ptr(&self, addr: usize) -> usize {
        self.read::<usize>(addr).unwrap_or(0)
    }
}

/// This process's memory.
pub struct Process;

impl Memory for Process {
    fn read<T: Copy + Default>(&self, addr: usize) -> Option<T> {
        read(addr)
    }
}

pub fn exe_base() -> usize {
    // SAFETY: `GetModuleHandleW(None)` only looks up the exe's own handle.
    unsafe { GetModuleHandleW(None) }
        .map(|h| h.0 as usize)
        .unwrap_or(0)
}

/// Reads a value from game memory. Returns `None` instead of crashing if the
/// address is not readable.
pub fn read<T: Copy + Default>(addr: usize) -> Option<T> {
    if addr == 0 {
        return None;
    }
    let mut value = T::default();
    let mut done = 0usize;
    // SAFETY: the destination is a local of exactly `size_of::<T>()` bytes;
    // `ReadProcessMemory` checks the source address.
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            addr as *const c_void,
            &mut value as *mut T as *mut c_void,
            size_of::<T>(),
            Some(&mut done),
        )
    };
    let value = (ok.is_ok() && done == size_of::<T>()).then_some(value)?;
    // Stress test only: the game's data looks hostile to us (never changes
    // the game's own memory).
    #[cfg(feature = "dev")]
    let value = crate::chaos::hostile_read(value)?;
    #[cfg(feature = "dev")]
    if crate::chaos::site_read_blocked(addr) {
        return None;
    }
    Some(value)
}

/// Reads a pointer; 0 if unreadable.
pub fn read_ptr(addr: usize) -> usize {
    read::<usize>(addr).unwrap_or(0)
}

/// Writes a value to game memory; false if the address is not writable.
pub fn write<T: Copy>(addr: usize, value: T) -> bool {
    #[cfg(feature = "dev")]
    if crate::chaos::roll(crate::chaos::Fault::WriteFail) {
        return false;
    }
    let mut done = 0usize;
    // SAFETY: the source is a local of exactly `size_of::<T>()` bytes;
    // `WriteProcessMemory` checks the destination address.
    let ok = unsafe {
        WriteProcessMemory(
            GetCurrentProcess(),
            addr as *const c_void,
            &value as *const T as *const c_void,
            size_of::<T>(),
            Some(&mut done),
        )
    };
    ok.is_ok() && done == size_of::<T>()
}

/// Copies `len` bytes of game memory into a buffer.
pub fn read_bytes(addr: usize, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut done = 0usize;
    // SAFETY: the destination buffer is `len` bytes long;
    // `ReadProcessMemory` checks the source address.
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            addr as *const c_void,
            buf.as_mut_ptr() as *mut c_void,
            len,
            Some(&mut done),
        )
    };
    (ok.is_ok() && done == len).then_some(buf)
}

/// True if `size` bytes at `addr` stay inside one 64-byte cache line. Only
/// then is a store of that size atomic on x86-64.
pub fn in_one_cache_line(addr: usize, size: usize) -> bool {
    addr % 64 + size <= 64
}

/// Makes `size` bytes of code at `addr` writable, runs `store`, then restores
/// the protection and flushes the instruction cache. False if the page could
/// not be made writable.
///
/// # Safety
/// `store` must write only inside those `size` bytes.
unsafe fn with_writable_code(addr: usize, size: usize, store: impl FnOnce()) -> bool {
    let mut old = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: changes the protection of existing code pages only.
    if unsafe {
        VirtualProtect(
            addr as *const c_void,
            size,
            PAGE_EXECUTE_READWRITE,
            &mut old,
        )
    }
    .is_err()
    {
        return false;
    }
    store();
    let mut ignored = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: puts back the protection read above, then flushes the bytes
    // just written.
    unsafe {
        let _ = VirtualProtect(addr as *const c_void, size, old, &mut ignored);
        let _ = FlushInstructionCache(GetCurrentProcess(), Some(addr as *const c_void), size);
    }
    true
}

/// Writes 2 bytes of code as a single 16-bit store, if they still hold
/// `from`. Returns true if the code now holds `to`.
///
/// A game thread may be running this exact code at the same moment. A store
/// that does not cross a cache line is atomic on x86-64, so the game sees
/// either the old or the new instruction. (A plain `memcpy` could write byte
/// by byte, and a half-written instruction would decode as something else.)
/// If another mod has changed these bytes since, they are left alone.
pub fn write_code_u16(addr: usize, from: u16, to: u16) -> bool {
    #[cfg(feature = "dev")]
    if crate::chaos::roll(crate::chaos::Fault::WriteFail) {
        return false;
    }
    if read::<u16>(addr) != Some(from) || !in_one_cache_line(addr, 2) {
        return false;
    }
    // SAFETY: one 2-byte store at `addr`, inside the range made writable.
    let ok = unsafe {
        with_writable_code(addr, 2, || {
            std::arch::asm!(
                "mov word ptr [{addr}], {value:x}",
                addr = in(reg) addr,
                value = in(reg) to,
                options(nostack, preserves_flags),
            )
        })
    };
    ok && read::<u16>(addr) == Some(to)
}

/// Like `write_code_u16`, for 4 bytes (a `rel32`).
pub fn write_code_u32(addr: usize, from: u32, to: u32) -> bool {
    #[cfg(feature = "dev")]
    if crate::chaos::roll(crate::chaos::Fault::WriteFail) {
        return false;
    }
    if read::<u32>(addr) != Some(from) || !in_one_cache_line(addr, 4) {
        return false;
    }
    // SAFETY: one 4-byte store at `addr`, inside the range made writable.
    let ok = unsafe {
        with_writable_code(addr, 4, || {
            std::arch::asm!(
                "mov dword ptr [{addr}], {value:e}",
                addr = in(reg) addr,
                value = in(reg) to,
                options(nostack, preserves_flags),
            )
        })
    };
    ok && read::<u32>(addr) == Some(to)
}

/// Target of a `call/jmp rel32` whose opcode is at `at`.
pub fn rel32_target(at: usize) -> Option<usize> {
    let rel = read::<i32>(at + 1)?;
    Some((at as isize + 5 + rel as isize) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_line() {
        assert!(in_one_cache_line(0x1000, 2));
        assert!(in_one_cache_line(0x103e, 2));
        assert!(!in_one_cache_line(0x103f, 2));
        assert!(in_one_cache_line(0x103c, 4));
        assert!(!in_one_cache_line(0x103d, 4));
    }

    #[test]
    fn reads_own_memory() {
        let value = 0x1234_5678u32;
        assert_eq!(read::<u32>(&value as *const u32 as usize), Some(value));
        assert_eq!(read::<u32>(0), None);
    }

    #[test]
    fn rel32() {
        // call rel32 = -5: a call to itself.
        let code = [0xE8u8, 0xFB, 0xFF, 0xFF, 0xFF];
        let at = code.as_ptr() as usize;
        assert_eq!(rel32_target(at), Some(at));
    }

    #[test]
    fn code_write_is_refused_when_the_bytes_are_not_the_expected_ones() {
        let page = crate::testutil::Page::new();
        page.put(0x10, [0x90u8, 0x90]);
        let addr = page.addr() + 0x10;
        assert!(!write_code_u16(addr, 0xD233, 0x01B2), "another mod's bytes");
        assert_eq!(page.get::<[u8; 2]>(0x10), [0x90, 0x90]);
        page.put(0x10, [0x33u8, 0xD2]);
        assert!(write_code_u16(addr, 0xD233, 0x01B2));
        assert_eq!(page.get::<[u8; 2]>(0x10), [0xB2, 0x01]);
        assert!(!write_code_u16(addr, 0xD233, 0x01B2), "already changed");
    }

    #[test]
    fn code_write_across_a_cache_line_is_refused() {
        let page = crate::testutil::Page::new();
        page.put(0x3f, [0x33u8, 0xD2]);
        assert!(!write_code_u16(page.addr() + 0x3f, 0xD233, 0x01B2));
        assert_eq!(page.get::<[u8; 2]>(0x3f), [0x33, 0xD2]);
    }

    #[test]
    fn unreadable_memory_reads_as_none() {
        let mut page = crate::testutil::Page::new();
        let addr = page.addr();
        page.release();
        assert_eq!(read::<u32>(addr), None);
        assert!(!write::<u8>(addr, 1));
        assert_eq!(read_ptr(addr), 0);
    }
}

//! Part 1 of the fix: the respawn condition's weather check, switched off
//! while a death respawn is pending (see `fix.rs`).

use crate::mem::{in_one_cache_line, read, write_code_u16};
use crate::scan::{Pattern, find_unique, has_bytes, pattern};

/// The respawn condition's call to `FieldArea::IsStable`.
///
/// ```text
/// 48 8B 0D ?? ?? ?? ??        mov  rcx, [rip + CS::FieldArea]
/// 48 85 C9                    test rcx, rcx
/// 74 ??                       je   skip
/// 33 D2                       xor  edx, edx                 <- patched
/// E8 ?? ?? ?? ??              call FieldArea::IsStable
/// 84 C0                       test al, al
/// 74 ??                       je   not_yet
/// 44 8B 47 10                 mov  r8d, [rdi + 0x10]
/// 45 33 C9                    xor  r9d, r9d
/// 48 8B CB                    mov  rcx, rbx
/// C7 44 24 20 00 00 00 00     mov  dword [rsp + 0x20], 0
/// 41 8D 51 48                 lea  edx, [r9 + 0x48]         ; action 0x48: respawn
/// ```
const RESPAWN_CONDITION: &Pattern = pattern!(
    "48 8B 0D ?? ?? ?? ?? 48 85 C9 74 ?? 33 D2 E8 ?? ?? ?? ?? 84 C0 74 ?? \
     44 8B 47 10 45 33 C9 48 8B CB C7 44 24 20 00 00 00 00 41 8D 51 48"
);

/// Where the `xor edx, edx` starts inside the pattern.
const PATCH_OFFSET: usize = 12;

/// `xor edx, edx`: second argument = 0, wait for the weather (game default).
const ORIGINAL: [u8; 2] = [0x33, 0xD2];

/// `mov dl, 1`: second argument = 1, skip the weather check.
const PATCHED: [u8; 2] = [0xB2, 0x01];

const _: () = assert!(has_bytes(RESPAWN_CONDITION, PATCH_OFFSET, &ORIGINAL));

/// The 2-byte instruction we switch between `ORIGINAL` and `PATCHED`.
pub struct PatchSite {
    pub addr: usize,
    /// The code holds `PATCHED`.
    pub active: bool,
}

impl PatchSite {
    /// Finds the instruction to patch in the respawn condition.
    pub fn find() -> Result<Self, String> {
        let addr = find_unique(RESPAWN_CONDITION, "respawn condition")? + PATCH_OFFSET;
        if !in_one_cache_line(addr, ORIGINAL.len()) {
            return Err("the instruction crosses a cache line, cannot patch it atomically".into());
        }
        Ok(Self {
            addr,
            active: false,
        })
    }

    fn write(&self, from: [u8; 2], to: [u8; 2]) -> bool {
        write_code_u16(self.addr, u16::from_le_bytes(from), u16::from_le_bytes(to))
    }

    /// Skips the weather check (call while a respawn is pending).
    ///
    /// `active` always follows the bytes actually in memory, not whether the
    /// write reported success.
    pub fn enable(&mut self) {
        if !self.active {
            self.write(ORIGINAL, PATCHED);
            self.active = read::<[u8; 2]>(self.addr) == Some(PATCHED);
        }
    }

    /// Restores the game's original code.
    pub fn disable(&mut self) {
        if self.active {
            self.write(PATCHED, ORIGINAL);
            self.active = read::<[u8; 2]>(self.addr) == Some(PATCHED);
        }
    }
}

/// If our thread ever stops (an internal error), the game gets its original
/// code back.
impl Drop for PatchSite {
    fn drop(&mut self) {
        self.disable();
    }
}

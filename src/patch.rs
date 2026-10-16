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
    /// The code is known to hold `PATCHED`.
    pub active: bool,
    /// A write was attempted and the original bytes are not confirmed back:
    /// the code may hold `PATCHED` even if a read could not say so. `disable`
    /// keeps retrying while this is set.
    dirty: bool,
}

impl PatchSite {
    /// Finds the instruction to patch in the respawn condition.
    pub fn find() -> Result<Self, String> {
        let addr = find_unique(RESPAWN_CONDITION, "respawn condition")? + PATCH_OFFSET;
        if !in_one_cache_line(addr, ORIGINAL.len()) {
            return Err("the instruction crosses a cache line, cannot patch it atomically".into());
        }
        Ok(Self::at(addr))
    }

    /// A site at `addr`, holding the original bytes.
    pub fn at(addr: usize) -> Self {
        Self {
            addr,
            active: false,
            dirty: false,
        }
    }

    fn write(&self, from: [u8; 2], to: [u8; 2]) -> bool {
        write_code_u16(self.addr, u16::from_le_bytes(from), u16::from_le_bytes(to))
    }

    /// Skips the weather check (call while a respawn is pending).
    ///
    /// `active` follows the bytes actually in memory, not whether the write
    /// reported success. The site is marked dirty before the write, so even if
    /// the read-back fails the code is restored later.
    pub fn enable(&mut self) {
        if !self.active {
            self.dirty = true;
            self.write(ORIGINAL, PATCHED);
            // Stress test only: the read-back right after the write fails.
            #[cfg(feature = "dev")]
            crate::chaos::arm_readback_fail(self.addr);
            self.active = read::<[u8; 2]>(self.addr) == Some(PATCHED);
        }
    }

    /// Restores the game's original code. Safe to call every tick: the write
    /// only happens if the bytes are still ours, and a failed read or write
    /// leaves the site dirty so the next call tries again.
    pub fn disable(&mut self) {
        if !(self.active || self.dirty) {
            return;
        }
        self.write(PATCHED, ORIGINAL);
        match read::<[u8; 2]>(self.addr) {
            Some(bytes) if bytes == ORIGINAL => {
                self.active = false;
                self.dirty = false;
            }
            Some(bytes) => self.active = bytes == PATCHED,
            None => self.active = false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Page;

    fn site_on_page() -> (Page, PatchSite) {
        let page = Page::new();
        page.put(0x100, ORIGINAL);
        let site = PatchSite::at(page.addr() + 0x100);
        (page, site)
    }

    #[test]
    fn restore_works_even_if_the_read_back_failed() {
        // The site was patched but a read could not confirm it (`active` is
        // false): `disable` must still put the original bytes back.
        let (page, mut site) = site_on_page();
        page.put(0x100, PATCHED);
        site.dirty = true;
        site.active = false;
        site.disable();
        assert_eq!(page.get::<[u8; 2]>(0x100), ORIGINAL);
        assert!(!site.dirty && !site.active);
    }

    #[test]
    fn a_clean_site_never_writes() {
        // Another mod's bytes at the site: a clean site must not touch them.
        let (page, mut site) = site_on_page();
        page.put(0x100, [0x90u8, 0x90]);
        site.disable();
        assert_eq!(page.get::<[u8; 2]>(0x100), [0x90, 0x90]);
    }

    #[test]
    fn enable_then_disable_round_trips() {
        let (page, mut site) = site_on_page();
        site.enable();
        assert!(site.active);
        assert_eq!(page.get::<[u8; 2]>(0x100), PATCHED);
        site.disable();
        assert!(!site.active && !site.dirty);
        assert_eq!(page.get::<[u8; 2]>(0x100), ORIGINAL);
    }

    /// The exact trigger of the stuck-patch bug: the read-back right after the
    /// write fails, so the site does not know it is patched. `disable` must still
    /// restore it.
    #[cfg(feature = "dev")]
    #[test]
    fn a_failed_readback_after_the_write_still_gets_restored() {
        use crate::chaos;
        let (page, mut site) = site_on_page();
        chaos::configure(chaos::parse_modes("readback_fail"), 0, 1);
        chaos::set_count(2);
        site.enable();
        assert_eq!(
            page.get::<[u8; 2]>(0x100),
            PATCHED,
            "the write itself worked"
        );
        assert!(
            !site.active,
            "the read-back failed, so the site thinks it is clean"
        );
        // The loop calls `disable` every tick: the first call may still hit the
        // failing reads (it cannot verify the bytes), a later one restores.
        for _ in 0..5 {
            site.disable();
        }
        chaos::configure(0, 0, 0);
        assert_eq!(page.get::<[u8; 2]>(0x100), ORIGINAL);
    }
}

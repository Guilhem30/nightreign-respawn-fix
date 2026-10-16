//! Test-only fault injection (builds with the `dev` feature only).
//!
//! The stress test (`tools/repro/stress.py`) turns these on through the ini
//! (`chaos = read_fail,garbage,...`, `chaos_rate = <per mille>`,
//! `chaos_seed = <n>`) to make the mod's own view of the game hostile: reads
//! fail or return random bytes, writes are refused, the respawn state flaps,
//! and the on-screen message hook panics, sees a wrong layout or is late.
//! Faults are injected on our side of each access, so the game's own data is
//! never corrupted.
//!
//! Every function is lock-free and allocation-free: the hook on the game
//! thread uses them too.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// A kind of fault.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Fault {
    /// `mem::read` finds nothing.
    ReadFail = 0,
    /// `mem::read` returns random bytes.
    Garbage,
    /// The respawn-pending state is inverted.
    Flap,
    /// `mem::write` and the code patches are refused.
    WriteFail,
    /// The message hook panics on the game thread.
    BannerPanic,
    /// The message hook sees a text object with a wrong layout.
    BannerLayout,
    /// A message request is old by the time the HUD update runs.
    BannerSlow,
    /// The fix thread panics (an "internal error": must stop the mod, not the
    /// game, and put the game's code back).
    FixPanic,
    /// Targeted: the first reads of the patch site right after the patch is
    /// written fail (the read-back that used to decide whether the patch was
    /// active).
    ReadbackFail,
    /// Targeted: the scenery block map is unreadable for the first reads at the
    /// end of a respawn, when the fade flags are cleared.
    MapUnreadable,
}

const FAULTS: [(&str, Fault); 10] = [
    ("read_fail", Fault::ReadFail),
    ("garbage", Fault::Garbage),
    ("flap", Fault::Flap),
    ("write_fail", Fault::WriteFail),
    ("banner_panic", Fault::BannerPanic),
    ("banner_layout", Fault::BannerLayout),
    ("banner_slow", Fault::BannerSlow),
    ("fix_panic", Fault::FixPanic),
    ("readback_fail", Fault::ReadbackFail),
    ("map_unreadable", Fault::MapUnreadable),
];

/// The state of one injector: which faults are on, how often, and how many
/// were injected.
pub struct Injector {
    modes: AtomicU32,
    /// Chance of a fault per access, in 1/1000.
    rate: AtomicU32,
    state: AtomicU64,
    counts: [AtomicU64; 10],
}

impl Injector {
    pub const fn new() -> Self {
        Self {
            modes: AtomicU32::new(0),
            rate: AtomicU32::new(0),
            state: AtomicU64::new(0x9E37_79B9_7F4A_7C15),
            counts: [const { AtomicU64::new(0) }; 10],
        }
    }

    /// Replaces the settings. A seed of 0 keeps the default one.
    pub fn configure(&self, modes: u32, rate: u32, seed: u64) {
        self.modes.store(modes, Ordering::Relaxed);
        self.rate.store(rate.min(1000), Ordering::Relaxed);
        if seed != 0 {
            self.state.store(seed, Ordering::Relaxed);
        }
    }

    /// xorshift64*; the state is shared by all threads.
    fn next(&self) -> u64 {
        let mut x = self.state.load(Ordering::Relaxed);
        loop {
            let mut n = x;
            n ^= n >> 12;
            n ^= n << 25;
            n ^= n >> 27;
            match self
                .state
                .compare_exchange_weak(x, n, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return n.wrapping_mul(0x2545_F491_4F6C_DD1D),
                Err(current) => x = current,
            }
        }
    }

    /// True if the mode `fault` is on; counts one injection.
    pub fn arm(&self, fault: Fault) -> bool {
        let on = self.modes.load(Ordering::Relaxed) & (1 << fault as u32) != 0;
        if on {
            self.counts[fault as usize].fetch_add(1, Ordering::Relaxed);
        }
        on
    }

    /// True if `fault` is on and this access draws it.
    pub fn roll(&self, fault: Fault) -> bool {
        if self.modes.load(Ordering::Relaxed) & (1 << fault as u32) == 0 {
            return false;
        }
        let hit = (self.next() % 1000) < u64::from(self.rate.load(Ordering::Relaxed));
        if hit {
            self.counts[fault as usize].fetch_add(1, Ordering::Relaxed);
        }
        hit
    }

    /// Overwrites `value` with random bytes if the `Garbage` fault hits.
    ///
    /// Only for plain data (integers, floats, arrays of them) read from game
    /// memory: every bit pattern must be a valid value of `T`.
    pub fn scramble<T: Copy>(&self, value: &mut T) {
        if !self.roll(Fault::Garbage) {
            return;
        }
        // SAFETY: `value` is a live `T`; the doc above requires every bit
        // pattern to be valid for it.
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(value as *mut T as *mut u8, size_of::<T>()) };
        for b in bytes {
            *b = self.next() as u8;
        }
    }

    /// How many times each fault was injected, as `name=count` pairs (only
    /// those that fired), or empty.
    pub fn counters(&self) -> String {
        FAULTS
            .iter()
            .filter_map(|(name, fault)| {
                let n = self.counts[*fault as usize].load(Ordering::Relaxed);
                (n > 0).then(|| format!("{name}={n}"))
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The injector the mod uses.
static INJECTOR: Injector = Injector::new();

/// How many reads a targeted fault blocks (`chaos_count`, default 2).
static COUNT: AtomicU32 = AtomicU32::new(2);
/// Patch site address and the reads of it still to fail.
static SITE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static SITE_LEFT: AtomicU32 = AtomicU32::new(0);
/// Reads of the block map still to fail.
static MAP_LEFT: AtomicU32 = AtomicU32::new(0);

pub fn set_count(n: u32) {
    COUNT.store(if n == 0 { 2 } else { n }, Ordering::Relaxed);
}

/// The patch was just written: make the next reads of `addr` fail.
pub fn arm_readback_fail(addr: usize) {
    if INJECTOR.arm(Fault::ReadbackFail) {
        SITE.store(addr, Ordering::Relaxed);
        SITE_LEFT.store(COUNT.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// True (and one failure used up) if a read of `addr` must fail.
pub fn site_read_blocked(addr: usize) -> bool {
    addr == SITE.load(Ordering::Relaxed) && take(&SITE_LEFT)
}

/// A respawn just ended: make the next reads of the block map fail.
pub fn arm_map_unreadable() {
    if INJECTOR.arm(Fault::MapUnreadable) {
        MAP_LEFT.store(COUNT.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// True (and one failure used up) if the block map must look unreadable.
pub fn map_blocked() -> bool {
    take(&MAP_LEFT)
}

fn take(left: &AtomicU32) -> bool {
    let mut n = left.load(Ordering::Relaxed);
    while n > 0 {
        match left.compare_exchange_weak(n, n - 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(current) => n = current,
        }
    }
    false
}

/// Parses `read_fail,garbage,...` (unknown names are ignored) into a mode
/// mask for `Injector::configure`.
pub fn parse_modes(text: &str) -> u32 {
    text.split(',')
        .map(str::trim)
        .filter_map(|name| FAULTS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)))
        .fold(0, |mask, (_, fault)| mask | 1 << *fault as u32)
}

pub fn configure(modes: u32, rate: u32, seed: u64) {
    INJECTOR.configure(modes, rate, seed);
}

pub fn roll(fault: Fault) -> bool {
    INJECTOR.roll(fault)
}

/// The `ReadFail` and `Garbage` faults applied to a value just read from the
/// game (same plain-data requirement as `Injector::scramble`).
pub fn hostile_read<T: Copy>(mut value: T) -> Option<T> {
    if INJECTOR.roll(Fault::ReadFail) {
        return None;
    }
    INJECTOR.scramble(&mut value);
    Some(value)
}

pub fn counters() -> String {
    INJECTOR.counters()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mode_names() {
        assert_eq!(parse_modes(""), 0);
        assert_eq!(parse_modes("read_fail"), 1);
        assert_eq!(parse_modes("Garbage, flap ,nope"), 0b110);
        assert_eq!(parse_modes("banner_slow"), 1 << 6);
        assert_eq!(parse_modes("fix_panic"), 1 << 7);
        assert_eq!(parse_modes("readback_fail,map_unreadable"), 0b11 << 8);
    }

    #[test]
    fn rate_zero_never_and_full_always() {
        let i = Injector::new();
        i.configure(parse_modes("read_fail"), 0, 7);
        assert!((0..1000).all(|_| !i.roll(Fault::ReadFail)));
        i.configure(parse_modes("read_fail"), 1000, 7);
        assert!((0..1000).all(|_| i.roll(Fault::ReadFail)));
        assert!(!i.roll(Fault::Garbage), "a mode that is off never fires");
        assert_eq!(i.counters(), "read_fail=1000");
    }

    #[test]
    fn same_seed_same_faults() {
        let run = |seed| {
            let i = Injector::new();
            i.configure(parse_modes("flap"), 300, seed);
            (0..200).map(|_| i.roll(Fault::Flap)).collect::<Vec<_>>()
        };
        assert_eq!(run(42), run(42));
        assert_ne!(run(42), run(43));
    }

    #[test]
    fn scramble_changes_plain_data_only_when_it_fires() {
        let i = Injector::new();
        let mut v = [0x11u8; 16];
        i.scramble(&mut v);
        assert_eq!(v, [0x11; 16]);
        i.configure(parse_modes("garbage"), 1000, 5);
        i.scramble(&mut v);
        assert_ne!(v, [0x11; 16]);
    }
}

//! The code that runs on the game thread: `hud_update` replaces the HUD's
//! per-frame update, calls the original, then shows a pending message.
//!
//! This is the only code of ours on a game thread, so it is defensive:
//! a request that is too old is dropped, the game's text object and manager
//! are checked before use, a panic is caught, and after a panic or a layout
//! mismatch the hook does nothing more. Each outcome is reported to our own
//! thread through `take_status` (a game thread must not log).

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use windows::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_PROTECTION_FLAGS, PAGE_READWRITE,
    VirtualAlloc, VirtualProtect,
};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::mem::read;

pub const KIND_PLAIN: u64 = 1;
pub const KIND_AVOIDED: u64 = 2;
pub const KIND_NOT_AVOIDED: u64 = 3;

/// A request older than this is dropped instead of shown (the player may have
/// left to the title screen, and the HUD update did not run for a while).
const MAX_AGE_MS: u32 = 5_000;

/// What happened to the latest request, as seen by the game thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Status {
    /// Handed to the game's banner code.
    Shown = 1,
    /// Dropped: too old when the HUD update ran.
    Stale,
    /// The game's text object does not look as expected; the hook is off.
    LayoutMismatch,
    /// The banner manager was not readable; this request was dropped.
    BadManager,
    /// A panic was caught; the hook is off.
    Panicked,
}

impl Status {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Shown),
            2 => Some(Self::Stale),
            3 => Some(Self::LayoutMismatch),
            4 => Some(Self::BadManager),
            5 => Some(Self::Panicked),
            _ => None,
        }
    }

    /// One line for the log.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Shown => "message handed to the game",
            Self::Stale => "message dropped: the HUD did not update in time",
            Self::LayoutMismatch => {
                "messages off: the game's text object has an unexpected layout (game updated?)"
            }
            Self::BadManager => "message dropped: the banner manager is not readable",
            Self::Panicked => "messages off: an internal error was caught on the game thread",
        }
    }
}

/// Request from our thread to the game thread (0 = nothing to show), see
/// `pack`.
static PENDING: AtomicU64 = AtomicU64::new(0);
/// The latest `Status` (0 = nothing new since `take_status`).
static STATUS: AtomicU8 = AtomicU8::new(0);
/// Set after a panic or a layout mismatch: no more requests are shown.
static DISABLED: AtomicBool = AtomicBool::new(false);
/// Time zero of the request timestamps.
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Game functions and texts, set once by `init` before the hook is installed.
pub struct Targets {
    pub original_update: usize,
    pub show: usize,
    pub show_with_text: usize,
    pub text_ctor: usize,
    pub manager_offset: usize,
}

static ORIGINAL_UPDATE: AtomicUsize = AtomicUsize::new(0);
static SHOW: AtomicUsize = AtomicUsize::new(0);
static SHOW_WITH_TEXT: AtomicUsize = AtomicUsize::new(0);
static TEXT_CTOR: AtomicUsize = AtomicUsize::new(0);
static MANAGER_OFFSET: AtomicUsize = AtomicUsize::new(0);
/// NUL-terminated UTF-16 texts (leaked, live as long as the game).
static TEXT_AVOIDED: AtomicUsize = AtomicUsize::new(0);
static TEXT_NOT_AVOIDED: AtomicUsize = AtomicUsize::new(0);

/// Stores the game functions and our texts for `hud_update`.
pub fn init(t: &Targets, avoided: &str, not_avoided: &str) {
    EPOCH.get_or_init(Instant::now);
    MANAGER_OFFSET.store(t.manager_offset, Ordering::Relaxed);
    SHOW.store(t.show, Ordering::Relaxed);
    SHOW_WITH_TEXT.store(t.show_with_text, Ordering::Relaxed);
    TEXT_CTOR.store(t.text_ctor, Ordering::Relaxed);
    ORIGINAL_UPDATE.store(t.original_update, Ordering::Relaxed);
    TEXT_AVOIDED.store(utf16_leaked(avoided), Ordering::Relaxed);
    TEXT_NOT_AVOIDED.store(utf16_leaked(not_avoided), Ordering::Relaxed);
}

/// Milliseconds since `init`, truncated to 24 bits.
fn now_stamp() -> u32 {
    EPOCH
        .get()
        .map_or(0, |epoch| epoch.elapsed().as_millis() as u32 & STAMP_MASK)
}

const STAMP_MASK: u32 = 0x00FF_FFFF;

/// Packs a request into one atomic word: bits 0-31 the text effect row, bits
/// 32-39 the kind (never 0), bits 40-63 the timestamp.
fn pack(kind: u64, row: u32, stamp: u32) -> u64 {
    u64::from(stamp & STAMP_MASK) << 40 | (kind & 0xFF) << 32 | u64::from(row)
}

/// Inverse of `pack`: `(kind, row, stamp)`.
fn unpack(word: u64) -> (u64, u32, u32) {
    ((word >> 32) & 0xFF, word as u32, (word >> 40) as u32)
}

/// True if a request stamped `then` is too old at `now` (both 24-bit, so the
/// subtraction wraps).
fn is_stale(then: u32, now: u32) -> bool {
    now.wrapping_sub(then) & STAMP_MASK > MAX_AGE_MS
}

/// Asks the game thread to show text effect `row` with text `kind` on its
/// next HUD update. Ignored once the hook has switched itself off.
pub fn request(kind: u64, row: u32) {
    if !DISABLED.load(Ordering::Relaxed) {
        let stamp = now_stamp();
        // Stress test only: the request is already too old when it is used.
        #[cfg(feature = "dev")]
        let stamp = if crate::chaos::roll(crate::chaos::Fault::BannerSlow) {
            stamp.wrapping_sub(MAX_AGE_MS + 1_000) & STAMP_MASK
        } else {
            stamp
        };
        PENDING.store(pack(kind, row, stamp), Ordering::Release);
    }
}

/// Stress test only: switches a hook that turned itself off back on, so each
/// scenario starts with a working one.
#[cfg(feature = "dev")]
pub fn rearm() {
    DISABLED.store(false, Ordering::Relaxed);
}

/// The latest outcome since the last call (only the newest is kept).
pub fn take_status() -> Option<Status> {
    Status::from_u8(STATUS.swap(0, Ordering::AcqRel))
}

fn report(status: Status) {
    STATUS.store(status as u8, Ordering::Release);
}

fn utf16_leaked(text: &str) -> usize {
    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    Box::leak(wide.into_boxed_slice()).as_ptr() as usize
}

/// Allocates `size` bytes of read-write memory within 2 GB below `near` (a
/// `rel32` must reach it), trying 64 KB steps.
fn alloc_near(near: usize, size: usize) -> Option<usize> {
    const STEP: usize = 0x1_0000;
    const RANGE: usize = 0x7000_0000;
    let mut addr = (near & !(STEP - 1)).checked_sub(STEP)?;
    let limit = near.saturating_sub(RANGE);
    while addr > limit {
        // SAFETY: asks for fresh memory at a free address; fails if taken.
        let p = unsafe {
            VirtualAlloc(
                Some(addr as *const c_void),
                size,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if !p.is_null() {
            return Some(p as usize);
        }
        addr -= STEP;
    }
    None
}

/// The bytes of a jump stub: `jmp [rip + 0]; dq target`.
fn stub_bytes(target: usize) -> [u8; 14] {
    let mut code = [0u8; 14];
    code[..6].copy_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
    code[6..].copy_from_slice(&(target as u64).to_le_bytes());
    code
}

/// Writes a jump stub to `hud_update` near `call`, then makes the page
/// read-execute (never writable and executable at once). Returns the stub's
/// address.
pub fn make_stub(call: usize) -> Option<usize> {
    const SIZE: usize = 0x1000;
    let stub = alloc_near(call, SIZE)?;
    let target = hud_update as unsafe extern "C" fn(usize, f32) as usize;
    let code = stub_bytes(target);
    // SAFETY: `stub` is a fresh, writable allocation of ours, larger than
    // `code`.
    unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), stub as *mut u8, code.len()) };
    let mut old = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: changes the protection of our own allocation; the flush covers
    // the bytes just written.
    unsafe {
        VirtualProtect(stub as *const c_void, SIZE, PAGE_EXECUTE_READ, &mut old).ok()?;
        let _ = FlushInstructionCache(GetCurrentProcess(), Some(stub as *const c_void), SIZE);
    }
    Some(stub)
}

/// The game's text object (0x40 bytes): `+0x00` a literal `const wchar_t*`
/// (the constructor sets it to a static `L""`), then an owned wide string
/// from +0x08 (offsets in the whole object: `+0x10` data pointer, `+0x18`
/// length, `+0x24` u16 bytes per character = 2), then an i64 at +0x38. The
/// banner shows the literal if it is non-empty, else the owned string if
/// non-empty, else the row's own text.
#[repr(C, align(16))]
struct TextArg([u64; 16]);

/// Runs on the game thread, in place of the HUD's per-frame update.
unsafe extern "C" fn hud_update(hud: usize, frame_time: f32) {
    // SAFETY: `init` stored the game's own update function before the hook
    // was installed; same signature.
    let original: unsafe extern "C" fn(usize, f32) =
        unsafe { std::mem::transmute(ORIGINAL_UPDATE.load(Ordering::Relaxed)) };
    // SAFETY: called with the game's own arguments.
    unsafe { original(hud, frame_time) };

    let word = PENDING.swap(0, Ordering::AcqRel);
    if word == 0 || DISABLED.load(Ordering::Relaxed) {
        return;
    }
    let (_, _, stamp) = unpack(word);
    if is_stale(stamp, now_stamp()) {
        report(Status::Stale);
        return;
    }
    // SAFETY: on the game thread, inside the HUD update (see `show_now`).
    let status = guarded(|| unsafe { show_now(hud, word) });
    report(status);
}

/// Runs `show` so that nothing can unwind into the game's code: a panic
/// becomes `Status::Panicked`. After a panic or a layout mismatch the hook is
/// switched off for good.
///
/// This is the safety boundary of the hook; the host tests exercise it
/// directly (a panic must come out as a status, never as an unwind).
fn guarded(show: impl FnOnce() -> Status + std::panic::UnwindSafe) -> Status {
    let status = std::panic::catch_unwind(show).unwrap_or(Status::Panicked);
    if matches!(status, Status::Panicked | Status::LayoutMismatch) {
        DISABLED.store(true, Ordering::Relaxed);
    }
    status
}

/// # Safety
/// On the game thread, with the HUD the game passed to its update.
unsafe fn show_now(hud: usize, word: u64) -> Status {
    let (kind, row, _) = unpack(word);
    // Stress tests only: a panic and a wrong layout on the game thread.
    #[cfg(feature = "dev")]
    {
        if crate::chaos::roll(crate::chaos::Fault::BannerPanic) {
            panic!("chaos: injected panic on the game thread");
        }
        if crate::chaos::roll(crate::chaos::Fault::BannerLayout) {
            return Status::LayoutMismatch;
        }
    }
    let manager = hud + MANAGER_OFFSET.load(Ordering::Relaxed);
    // The manager is a field of the HUD the game just updated; a failed read
    // means the HUD is not what we expect, so call nothing.
    if read::<usize>(manager).is_none() {
        return Status::BadManager;
    }
    let text = match kind {
        KIND_PLAIN => {
            // SAFETY: `show_text_effect(manager, row)`, found by `init`.
            unsafe {
                let show: unsafe extern "C" fn(usize, u32) =
                    std::mem::transmute(SHOW.load(Ordering::Relaxed));
                show(manager, row);
            }
            return Status::Shown;
        }
        KIND_AVOIDED => TEXT_AVOIDED.load(Ordering::Relaxed),
        KIND_NOT_AVOIDED => TEXT_NOT_AVOIDED.load(Ordering::Relaxed),
        // Not reachable: `request` is only called with the kinds above.
        _ => return Status::Stale,
    };

    // A text object built by the game's constructor, then its literal
    // pointer set to our string (the game keeps only that pointer; our
    // string lives as long as the game). The object owns nothing else and is
    // never destroyed.
    let mut arg = TextArg([0; 16]);
    let words = arg.0.as_mut_ptr();
    // SAFETY: the game's constructor, on a 0x80-byte aligned buffer (larger
    // than the 0x40-byte object).
    unsafe {
        let ctor: unsafe extern "C" fn(*mut u64) -> usize =
            std::mem::transmute(TEXT_CTOR.load(Ordering::Relaxed));
        ctor(words);
    }
    // Layout check: an empty literal and an empty wide string.
    // SAFETY: `words` points into `arg`.
    let literal = unsafe { words.read() } as usize;
    if read::<u16>(literal) != Some(0) || read::<u16>(words as usize + 0x24) != Some(2) {
        return Status::LayoutMismatch;
    }
    // SAFETY: `words` points into `arg`; the game calls with the object it
    // expects (checked above).
    unsafe {
        words.write(text as u64);
        let show_with_text: unsafe extern "C" fn(usize, u32, *mut u64) =
            std::mem::transmute(SHOW_WITH_TEXT.load(Ordering::Relaxed));
        show_with_text(manager, row, words);
    }
    Status::Shown
}

#[cfg(test)]
mod tests {
    use windows::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};

    use super::*;

    #[test]
    fn request_word_round_trips() {
        let word = pack(KIND_NOT_AVOIDED, 4200, 0x12_3456);
        assert_eq!(unpack(word), (KIND_NOT_AVOIDED, 4200, 0x12_3456));
        assert_ne!(word, 0, "a real request is never the empty word");
    }

    #[test]
    fn stamp_is_truncated_to_24_bits() {
        let (_, _, stamp) = unpack(pack(KIND_PLAIN, 1, 0xFFFF_FFFF));
        assert_eq!(stamp, STAMP_MASK);
    }

    #[test]
    fn staleness_handles_wraparound() {
        assert!(!is_stale(1_000, 1_000));
        assert!(!is_stale(1_000, 1_000 + MAX_AGE_MS));
        assert!(is_stale(1_000, 1_000 + MAX_AGE_MS + 1));
        // The 24-bit clock wrapped between request and use.
        assert!(!is_stale(STAMP_MASK - 10, 20));
        assert!(is_stale(STAMP_MASK - 10, MAX_AGE_MS + 100));
    }

    #[test]
    fn status_round_trips() {
        for s in [
            Status::Shown,
            Status::Stale,
            Status::LayoutMismatch,
            Status::BadManager,
            Status::Panicked,
        ] {
            assert_eq!(Status::from_u8(s as u8), Some(s));
        }
        assert_eq!(Status::from_u8(0), None);
    }

    #[test]
    fn stub_jumps_through_an_absolute_address() {
        let code = stub_bytes(0x1122_3344_5566_7788);
        assert_eq!(&code[..6], &[0xFF, 0x25, 0, 0, 0, 0]);
        assert_eq!(
            u64::from_le_bytes(code[6..].try_into().unwrap()),
            0x1122_3344_5566_7788
        );
    }

    #[test]
    fn stub_lands_in_rel32_range_and_is_executable_not_writable() {
        let near = stub_bytes as *const () as usize;
        let stub = make_stub(near).expect("allocation near this test binary");
        assert!(near.abs_diff(stub) < 0x7FFF_0000);
        assert_eq!(read::<u16>(stub), Some(0x25FF));
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: queries our own allocation into a local.
        let len = unsafe {
            VirtualQuery(
                Some(stub as *const c_void),
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        assert_ne!(len, 0);
        assert_eq!(info.Protect, PAGE_EXECUTE_READ);
    }

    #[test]
    fn any_request_word_decodes_and_ages_without_panic() {
        use crate::testutil::Rng;
        let mut rng = Rng(0xC0FFEE);
        for _ in 0..10_000 {
            let word = rng.next();
            let (kind, row, stamp) = unpack(word);
            assert!(stamp <= STAMP_MASK && kind <= 0xFF);
            let _ = is_stale(stamp, rng.next() as u32 & STAMP_MASK);
            assert_eq!(unpack(pack(kind, row, stamp)), (kind, row, stamp));
        }
    }

    #[test]
    fn every_status_byte_is_handled() {
        for b in 0..=255u8 {
            if let Some(s) = Status::from_u8(b) {
                assert_eq!(s as u8, b);
                assert!(!s.describe().is_empty());
            }
        }
    }

    /// Tests that touch the hook's globals run one at a time.
    static GLOBALS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    #[test]
    fn a_panic_becomes_a_status_and_switches_the_hook_off() {
        let _own = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        DISABLED.store(false, Ordering::Relaxed);
        let status = guarded(|| panic!("injected: game thread"));
        assert_eq!(status, Status::Panicked);
        assert!(DISABLED.load(Ordering::Relaxed));
        // Once off, no request is accepted.
        PENDING.store(0, Ordering::Relaxed);
        request(KIND_AVOIDED, 4200);
        assert_eq!(PENDING.load(Ordering::Relaxed), 0);
        DISABLED.store(false, Ordering::Relaxed);
    }

    #[test]
    fn a_layout_mismatch_switches_the_hook_off_but_a_shown_message_does_not() {
        let _own = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        DISABLED.store(false, Ordering::Relaxed);
        assert_eq!(guarded(|| Status::Shown), Status::Shown);
        assert!(!DISABLED.load(Ordering::Relaxed));
        assert_eq!(guarded(|| Status::Stale), Status::Stale);
        assert!(!DISABLED.load(Ordering::Relaxed));
        assert_eq!(guarded(|| Status::LayoutMismatch), Status::LayoutMismatch);
        assert!(DISABLED.load(Ordering::Relaxed));
        DISABLED.store(false, Ordering::Relaxed);
    }
}

//! # Respawn Gate Fix for Elden Ring Nightreign
//!
//! Fixes the long (~15 s extra) black screen after dying when the respawn
//! point is in an area with a different weather than the place you died
//! (most visible during the Night's Tide, when the rain kills you and you
//! respawn far away). What goes wrong and how the fix works: `fix.rs`.
//!
//! ## Modules
//!
//! * `fix`: the main loop, run on one background thread of our own.
//! * `patch`: part 1, the weather check switched off during a respawn.
//! * `death`: one death; part 2 (scenery fades finished), log summary.
//! * `game`, `geom`: the game state we read.
//! * `banner`: the on-screen message (the only code of ours on a game thread).
//! * `config`, `log`: `respawn_gate_fix.ini` and `respawn_gate_fix.log`.
//! * `mem`, `scan`: memory access and code patterns.
//!
//! ## Files (next to the DLL)
//!
//! * `respawn_gate_fix.ini`: settings, created with defaults on first run and
//!   re-read while the game runs:
//!   * `enabled = true`: the whole fix on or off,
//!   * `verbose = true`: detailed log entries (see below),
//!   * `notify = true`: an on-screen message after a respawn the fix had to
//!     act on (a weather transition or scenery fade): "Delayed respawn
//!     avoided", or "couldn't be avoided" if it still took long.
//! * `respawn_gate_fix.log`: one line per death with the time until the
//!   player came back. With `verbose`, a respawn that started a weather
//!   transition or took long also gets the details: where the player died,
//!   the respawn point, the weather before and after, the in-game clock, and
//!   what happened to the scenery fade. This beta collects that information to
//!   find the cases the fix does not cover yet.
//!
//! Builds with the `dev` feature also read test-only ini keys
//! (`banner_test`, `test_skip_fix`; see `config.rs`). Release builds ignore
//! them.

mod banner;
mod config;
mod death;
mod fix;
mod game;
mod geom;
mod log;
mod mem;
mod patch;
mod scan;

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

/// DLL entry point: starts the background thread and returns at once
/// (no real work may happen inside `DllMain`).
///
/// # Safety
/// Called by the Windows loader only.
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "system" fn DllMain(module: HMODULE, reason: u32, _: *mut ()) -> bool {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason != DLL_PROCESS_ATTACH {
        return true;
    }
    // Log and ini live next to the DLL.
    let mut buf = [0u16; 1024];
    // SAFETY: `module` is this DLL, `buf` a local buffer.
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    let dir = PathBuf::from(String::from_utf16_lossy(&buf[..len]))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::thread::Builder::new()
        .name("respawn-gate-fix".into())
        .spawn(move || {
            // An internal error stops the fix (the patch is restored on the way
            // out), never the game.
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fix::run(&dir)));
            if ran.is_err() {
                log::Log::open(&dir.join(fix::LOG_FILE))
                    .line("stopped after an internal error; the game's code is restored");
            }
        })
        .ok();
    true
}

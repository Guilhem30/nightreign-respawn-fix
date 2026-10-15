//! The fix and its main loop.
//!
//! ## What goes wrong
//!
//! After a death, an event script waits on a condition object
//! (`CS::CSLuaEventConditionStableAllEnd`) before it plays the respawn. Each
//! frame the condition asks "is the field area stable?", which is the AND of
//! five checks: map, collision, characters, world geometry, and area weather.
//!
//! When the respawn point has a different area weather, moving the player
//! there starts a 15 s weather transition, and two of those checks can wait
//! for it:
//!
//! * **weather**: the area weather counts as busy until the transition is
//!   over, including a 15 s lighting blend;
//! * **geometry**: the weather-dependent scenery block may fade over 15 s, and
//!   a block counts as "not ready" until its fade is over.
//!
//! So the respawn waits for the whole transition behind a black screen.
//!
//! ## The fix
//!
//! While a death respawn is pending, this DLL removes both waits:
//!
//! 1. **Weather check skipped** (`patch.rs`). The "is stable" function takes
//!    a second argument; when it is non-zero the weather check is skipped
//!    (other callers in the game use this). The respawn condition always
//!    passes 0:
//!
//!    ```text
//!    mov  rcx, [CS::FieldArea]
//!    test rcx, rcx
//!    je   ...
//!    xor  edx, edx            ; 33 D2   <- second argument = 0 ("also wait for the weather")
//!    call FieldArea::IsStable
//!    ```
//!
//!    The DLL replaces `xor edx, edx` with `mov dl, 1` (`B2 01`, same length)
//!    and puts the original bytes back as soon as the player is back. It is
//!    only patched during a death respawn because the same condition object
//!    may be used by other scripted events, which keep the original behaviour.
//!
//! 2. **Scenery fade finished** (`death.rs`, `geom.rs`). Each geometry block
//!    has an "instant" flag that the game checks during a fade: when it is
//!    set, the fade jumps to its end on the next frame. The DLL sets that
//!    flag on every block that is fading, and clears it again once the player
//!    is back (the game leaves the flag set, which would make later weather
//!    changes instant too). Sometimes the flag is already set; then there is
//!    nothing to do. This part is required: the geometry check waits for the
//!    fade, so skipping only the weather check still leaves a 15 s black
//!    screen.
//!
//! Map, collision, characters and the rest of the geometry still have to be
//! ready before the respawn, so you never respawn into unloaded terrain. The
//! lighting blend finishes after the screen fades in instead of before.
//!
//! ## Safety notes
//!
//! * The fix runs on one background thread of our own. Game data is read
//!   with `ReadProcessMemory`, so a stale pointer fails cleanly instead of
//!   crashing. An internal error stops that thread (and restores the code),
//!   not the game.
//! * The on-screen message is the only code of ours on a game thread: one
//!   redirected call to the HUD update (see `banner`), which shows a waiting
//!   message and otherwise only calls the original. It is installed only when
//!   a message is first needed (never with `notify = false`), drops requests
//!   that are too old, checks the game's objects before using them, catches
//!   a panic before it can reach the game's code, and switches itself off
//!   after a panic or an unexpected layout. What it did is logged.
//! * A scenery block is written only after it has been seen fading on two
//!   checks in a row, and its flag is cleared only if the block is still
//!   loaded, so a block unloaded in the meantime is never written.
//! * The code is only switched if it still holds the expected bytes, so a
//!   change made by another mod is left alone.
//! * Game memory is written only while a respawn is pending, and only the
//!   2-byte instruction and the one-byte fade flag described above; both are
//!   restored when the player is back.
//! * The code location is found with a byte pattern, not a fixed address, and
//!   must match exactly once. If a game update changes that code, the DLL logs
//!   it and does nothing.
//! * Each patch is one 2-byte store, so the game thread sees either the old or
//!   the new instruction, never half of each.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::banner::{self, Banner, Message};
use crate::config::Config;
use crate::death::Death;
use crate::game::Game;
use crate::log::Log;
use crate::mem::exe_base;
use crate::patch::PatchSite;

/// Mod name and version (from Cargo.toml), written at the top of each session
/// in the log.
const NAME: &str = "Respawn Gate Fix";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The summary is written this long after the respawn, once the player's
/// position has been updated (it still shows the death spot in the frame the
/// respawn ends).
const REPORT_DELAY: Duration = Duration::from_millis(500);

/// How often the background thread checks the respawn state. The game checks
/// the condition once per frame (~16 ms), so this is fast enough to be in
/// place long before the weather transition would matter (~1 s into the black).
const TICK: Duration = Duration::from_millis(10);

/// How often the ini file is checked for changes.
const CONFIG_CHECK: Duration = Duration::from_secs(1);

pub const LOG_FILE: &str = "respawn_gate_fix.log";
const INI_FILE: &str = "respawn_gate_fix.ini";

/// Runs the fix; `dir` holds the log and the ini. Returns only if the fix
/// cannot work in this game version.
pub fn run(dir: &Path) {
    let mut log = Log::open(&dir.join(LOG_FILE));
    let mut config = Config::open(dir.join(INI_FILE));
    log.line(&format!(
        "{NAME} {VERSION} loaded (pid {}), {}",
        std::process::id(),
        config.settings.describe()
    ));

    let mut site = match PatchSite::find() {
        Ok(site) => site,
        Err(e) => {
            log.line(&format!("not active: {e}"));
            return;
        }
    };
    log.line(&format!(
        "respawn condition found at exe+{:#x}",
        site.addr - exe_base()
    ));

    // The singletons are registered while the game boots.
    let game = loop {
        if let Some(game) = Game::find() {
            break game;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    log.line("ready");

    // On-screen messages are optional and hooked on first use only.
    let mut banner = BannerState::NotInstalled;

    let mut death: Option<Death> = None;
    let mut last_config_check = Instant::now();
    loop {
        std::thread::sleep(TICK);

        if last_config_check.elapsed() >= CONFIG_CHECK {
            last_config_check = Instant::now();
            if config.refresh() {
                log.line(&format!("settings changed: {}", config.settings.describe()));
                #[cfg(feature = "dev")]
                if let Some((row, text)) = config.settings.dev.banner_test {
                    log.line(&format!(
                        "banner test: row {row}, text {:?}",
                        text.map(Message::text)
                    ));
                    if let Some(banner) = banner.get(&mut log) {
                        banner.show_raw(row, text);
                    }
                }
            }
        }
        // What the game thread did with the last message.
        if let Some(status) = banner::take_status() {
            log.line(&format!("on-screen message: {}", status.describe()));
        }
        let settings = config.settings;
        let pending = game.respawn_pending();

        // Restore the original code as soon as the respawn is over (or the
        // fix gets disabled), before anything else.
        if !(pending && settings.enabled) {
            site.disable();
        }

        match (death.take(), pending) {
            // The player just died.
            (None, true) => death = Some(Death::new(&game)),
            // Respawn pending.
            (Some(mut d), true) if d.ended.is_none() => {
                d.update(&game, &settings, &mut site);
                death = Some(d);
            }
            // The player is back: undo the fade flags, note the time, report
            // a little later.
            (Some(mut d), false) if d.ended.is_none() => {
                d.clear_flags(&game);
                d.ended = Some(Instant::now());
                death = Some(d);
            }
            // Report once the delay is over (or at once if the player died
            // again in the meantime).
            (Some(d), pending) => {
                let ended = d.ended.unwrap_or_else(Instant::now);
                if pending || ended.elapsed() >= REPORT_DELAY {
                    let message = d.report(&game, &settings, &mut log);
                    // Not when the player already died again.
                    if let (Some(message), false, true) = (message, pending, settings.notify) {
                        log.line(&format!("on-screen message: {}", Message::text(message)));
                        if let Some(banner) = banner.get(&mut log) {
                            banner.show(message);
                        }
                    }
                } else {
                    death = Some(d);
                }
            }
            (None, false) => {}
        }
    }
}

/// The on-screen message hook, installed on first use.
enum BannerState {
    NotInstalled,
    Ready(Banner),
    /// Installing failed (logged once); the fix works without messages.
    Unavailable,
}

impl BannerState {
    /// The hook, installing it on the first call.
    fn get(&mut self, log: &mut Log) -> Option<&Banner> {
        if matches!(self, Self::NotInstalled) {
            *self = match Banner::install() {
                Ok(banner) => {
                    log.line("on-screen messages ready");
                    Self::Ready(banner)
                }
                Err(e) => {
                    log.line(&format!("on-screen messages off: {e}"));
                    Self::Unavailable
                }
            };
        }
        match self {
            Self::Ready(banner) => Some(banner),
            _ => None,
        }
    }
}

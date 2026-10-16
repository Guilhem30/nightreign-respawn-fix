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
use crate::config::{Config, Settings};
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

    let mut respawns = Respawns::new();
    let mut last_config_check = Instant::now();
    #[cfg(feature = "dev")]
    let mut last_chaos = String::new();
    loop {
        std::thread::sleep(TICK);
        // Stress test only: an internal error in this thread.
        #[cfg(feature = "dev")]
        if crate::chaos::roll(crate::chaos::Fault::FixPanic) {
            panic!("chaos: injected panic in the fix thread");
        }

        if last_config_check.elapsed() >= CONFIG_CHECK {
            last_config_check = Instant::now();
            // Stress test only: show that the injected faults really fired
            // (at most once a second, and only when something new happened).
            #[cfg(feature = "dev")]
            {
                let counters = crate::chaos::counters();
                if counters != last_chaos {
                    log.line(&format!("chaos injected: {counters}"));
                    last_chaos = counters;
                }
            }
            if config.refresh() {
                log.line(&format!("settings changed: {}", config.settings.describe()));
                #[cfg(feature = "dev")]
                banner::rearm();
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
        if let Some(message) = respawns.tick(&game, &settings, &mut site, &mut log) {
            if settings.notify {
                log.line(&format!("on-screen message: {}", Message::text(message)));
                if let Some(banner) = banner.get(&mut log) {
                    banner.show(message);
                }
            }
        }
    }
}

/// Follows the player's deaths: the weather check is skipped (and scenery
/// fades finished) while a respawn is pending, everything is undone when the
/// player is back, and a summary is written a little later.
struct Respawns {
    death: Option<Death>,
}

impl Respawns {
    const fn new() -> Self {
        Self { death: None }
    }

    /// One step of the state machine. Returns the on-screen message to show,
    /// if the respawn that just ended calls for one.
    fn tick(
        &mut self,
        game: &Game,
        settings: &Settings,
        site: &mut PatchSite,
        log: &mut Log,
    ) -> Option<Message> {
        let pending = game.respawn_pending();

        // Restore the original code as soon as the respawn is over (or the
        // fix gets disabled), before anything else.
        if !(pending && settings.enabled) {
            site.disable();
        }

        match (self.death.take(), pending) {
            // The player just died.
            (None, true) => self.death = Some(Death::new(game)),
            // Respawn pending.
            (Some(mut d), true) if d.ended.is_none() => {
                d.update(game, settings, site);
                self.death = Some(d);
            }
            // The player is back: undo the fade flags, note the time, report
            // a little later.
            (Some(mut d), false) if d.ended.is_none() => {
                // Stress test only: the block map is unreadable right now.
                #[cfg(feature = "dev")]
                crate::chaos::arm_map_unreadable();
                d.clear_flags(game);
                d.ended = Some(Instant::now());
                self.death = Some(d);
            }
            // Report once the delay is over (or at once if the player died
            // again in the meantime).
            (Some(mut d), pending) => {
                // Flags that could not be cleared yet (unreadable block map).
                d.clear_flags(game);
                let ended = d.ended.unwrap_or_else(Instant::now);
                if pending || ended.elapsed() >= REPORT_DELAY {
                    let message = d.report(game, settings, log);
                    // Not when the player already died again.
                    return message.filter(|_| !pending);
                }
                self.death = Some(d);
            }
            (None, false) => {}
        }
        None
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Page, Rng, World};

    const ORIGINAL: [u8; 2] = [0x33, 0xD2];

    fn temp_log(tag: &str) -> (Log, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("gatefix-test-{}-{tag}.log", std::process::id()));
        (Log::open(&path), path)
    }

    /// Code page with `xor edx, edx` at +0x100.
    fn code_site() -> (Page, PatchSite) {
        let page = Page::new();
        page.put(0x100, ORIGINAL);
        let site = PatchSite::at(page.addr() + 0x100);
        (page, site)
    }

    /// Makes the report delay of a finished death elapse.
    fn skip_report_delay(respawns: &mut Respawns) {
        if let Some(d) = respawns.death.as_mut().filter(|d| d.ended.is_some()) {
            d.ended = Some(Instant::now() - REPORT_DELAY);
        }
    }

    #[test]
    fn a_respawn_sets_then_restores_everything() {
        let world = World::new(2);
        let (page, mut site) = code_site();
        let (mut log, path) = temp_log("cycle");
        let mut respawns = Respawns::new();
        let settings = Settings::default();
        world.set_fading(1, true);

        world.set_pending(true);
        respawns.tick(&world.game, &settings, &mut site, &mut log); // death noticed
        assert_eq!(page.get::<[u8; 2]>(0x100), ORIGINAL);
        respawns.tick(&world.game, &settings, &mut site, &mut log); // patch, first sighting
        assert_eq!(page.get::<[u8; 2]>(0x100), [0xB2, 0x01]);
        assert_eq!(world.instant(1), 0, "never written on the first sighting");
        respawns.tick(&world.game, &settings, &mut site, &mut log); // seen twice
        assert_eq!(world.instant(1), 1);
        assert_eq!(
            world.instant(0),
            0,
            "a block that is not fading is left alone"
        );

        world.set_pending(false);
        respawns.tick(&world.game, &settings, &mut site, &mut log);
        assert_eq!(page.get::<[u8; 2]>(0x100), ORIGINAL, "code restored");
        assert_eq!(world.instant(1), 0, "flag cleared");

        skip_report_delay(&mut respawns);
        let message = respawns.tick(&world.game, &settings, &mut site, &mut log);
        assert_eq!(message, Some(Message::Avoided));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unreadable_game_state_does_nothing() {
        // Slots that point nowhere: no respawn is ever seen, nothing breaks.
        let game = Game::for_test(0xdead_0000_0000, 0xdead_0000_0008);
        let (page, mut site) = code_site();
        let (mut log, path) = temp_log("unreadable");
        let mut respawns = Respawns::new();
        for _ in 0..50 {
            assert_eq!(
                respawns.tick(&game, &Settings::default(), &mut site, &mut log),
                None
            );
        }
        assert_eq!(page.get::<[u8; 2]>(0x100), ORIGINAL);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_block_unloaded_before_the_flag_is_cleared_is_not_written() {
        let mut world = World::new(2);
        let (_page, mut site) = code_site();
        let (mut log, path) = temp_log("unloaded");
        let mut respawns = Respawns::new();
        let settings = Settings::default();
        world.set_fading(0, true);
        world.set_pending(true);
        for _ in 0..3 {
            respawns.tick(&world.game, &settings, &mut site, &mut log);
        }
        assert_eq!(world.instant(0), 1);

        // The expedition ends: the block leaves the map and its memory goes.
        world.link(&[1]);
        world.blocks[0].release();
        world.set_pending(false);
        respawns.tick(&world.game, &settings, &mut site, &mut log); // must not touch it
        skip_report_delay(&mut respawns);
        respawns.tick(&world.game, &settings, &mut site, &mut log);
        let _ = std::fs::remove_file(path);
    }

    /// Drives one death until the respawn is over.
    fn respawn_with(world: &World, site: &mut PatchSite, log: &mut Log, respawns: &mut Respawns) {
        let settings = Settings::default();
        world.set_pending(true);
        for _ in 0..4 {
            respawns.tick(&world.game, &settings, site, log);
        }
        world.set_pending(false);
        respawns.tick(&world.game, &settings, site, log);
    }

    #[test]
    fn a_block_that_left_the_map_is_not_written_even_if_its_memory_is_alive() {
        let world = World::new(2);
        let (_page, mut site) = code_site();
        let (mut log, path) = temp_log("left_map");
        let mut respawns = Respawns::new();
        world.set_fading(0, true);
        let settings = Settings::default();
        world.set_pending(true);
        for _ in 0..3 {
            respawns.tick(&world.game, &settings, &mut site, &mut log);
        }
        assert_eq!(world.instant(0), 1);
        // Block 0 leaves the map (its memory may be reused by the game); we
        // must not touch it again, even though it still holds our value 1.
        world.link(&[1]);
        world.set_pending(false);
        respawns.tick(&world.game, &settings, &mut site, &mut log);
        assert_eq!(world.instant(0), 1, "block outside the map was written");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_flag_the_game_changed_is_left_alone() {
        let world = World::new(1);
        let (_page, mut site) = code_site();
        let (mut log, path) = temp_log("changed");
        let mut respawns = Respawns::new();
        let settings = Settings::default();
        world.set_fading(0, true);
        world.set_pending(true);
        for _ in 0..3 {
            respawns.tick(&world.game, &settings, &mut site, &mut log);
        }
        assert_eq!(world.instant(0), 1);
        world.set_instant(0, 2); // the game wrote something else
        world.set_pending(false);
        respawns.tick(&world.game, &settings, &mut site, &mut log);
        assert_eq!(world.instant(0), 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_flag_set_by_the_game_is_never_cleared_by_us() {
        let world = World::new(1);
        let (_page, mut site) = code_site();
        let (mut log, path) = temp_log("game_flag");
        let mut respawns = Respawns::new();
        world.set_fading(0, true);
        world.set_instant(0, 1); // the game already made this fade instant
        respawn_with(&world, &mut site, &mut log, &mut respawns);
        assert_eq!(world.instant(0), 1, "we cleared a flag we did not set");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn flags_are_kept_while_the_map_is_unreadable_and_cleared_after() {
        let world = World::new(1);
        let (_page, mut site) = code_site();
        let (mut log, path) = temp_log("unreadable_map");
        let mut respawns = Respawns::new();
        let settings = Settings::default();
        world.set_fading(0, true);
        world.set_pending(true);
        for _ in 0..3 {
            respawns.tick(&world.game, &settings, &mut site, &mut log);
        }
        assert_eq!(world.instant(0), 1);

        world.set_map_readable(false);
        world.set_pending(false);
        respawns.tick(&world.game, &settings, &mut site, &mut log);
        assert_eq!(world.instant(0), 1, "still set: could not be cleared yet");

        world.set_map_readable(true);
        respawns.tick(&world.game, &settings, &mut site, &mut log);
        assert_eq!(world.instant(0), 0, "cleared once the map is readable");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dropping_the_site_restores_the_code() {
        let (page, mut site) = code_site();
        site.enable();
        assert_eq!(page.get::<[u8; 2]>(0x100), [0xB2, 0x01]);
        drop(site);
        assert_eq!(page.get::<[u8; 2]>(0x100), ORIGINAL);
    }

    /// Randomized chaos: the respawn state flaps, blocks start and stop
    /// fading, leave and re-enter the map, and pages vanish. Whatever
    /// happens, once the respawn is over the code is original and no live
    /// block keeps our flag.
    #[test]
    fn chaotic_engine_state_always_ends_clean() {
        const BLOCKS: usize = 4;
        for seed in 1..=25u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let world = World::new(BLOCKS);
            let (page, mut site) = code_site();
            let (mut log, path) = temp_log(&format!("chaos{seed}"));
            let mut respawns = Respawns::new();
            let settings = Settings::default();
            let mut linked: Vec<usize> = (0..BLOCKS).collect();
            let mut gone = [false; BLOCKS];
            let mut ever_unlinked = [false; BLOCKS];
            let mut world = world;

            for _ in 0..300 {
                match rng.below(8) {
                    0 => world.set_pending(rng.below(2) == 0),
                    1 | 2 => {
                        let i = rng.below(BLOCKS as u64) as usize;
                        if !gone[i] {
                            world.set_fading(i, rng.below(2) == 0);
                        }
                    }
                    3 => {
                        let i = rng.below(BLOCKS as u64) as usize;
                        if linked.contains(&i) {
                            linked.retain(|&j| j != i);
                            ever_unlinked[i] = true;
                        } else {
                            linked.push(i);
                            linked.sort_unstable();
                        }
                        world.link(&linked);
                    }
                    4 if rng.below(20) == 0 => {
                        let i = rng.below(BLOCKS as u64) as usize;
                        world.blocks[i].release();
                        gone[i] = true;
                    }
                    _ => {}
                }
                respawns.tick(&world.game, &settings, &mut site, &mut log);
                if rng.below(2) == 0 {
                    skip_report_delay(&mut respawns);
                }
            }

            // The respawn ends; let the tracker settle.
            world.set_pending(false);
            for _ in 0..4 {
                respawns.tick(&world.game, &settings, &mut site, &mut log);
                skip_report_delay(&mut respawns);
            }
            assert_eq!(page.get::<[u8; 2]>(0x100), ORIGINAL, "seed {seed}: code");
            // A block that stayed in the map must not keep our flag. (One that
            // left the map is the game's to reuse; we never write it again.)
            for (i, (&gone, &unlinked)) in gone.iter().zip(&ever_unlinked).enumerate() {
                if !gone && !unlinked {
                    assert_eq!(world.instant(i), 0, "seed {seed}: flag left on block {i}");
                }
            }
            let _ = std::fs::remove_file(path);
        }
    }
}

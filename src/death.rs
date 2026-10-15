//! One death, from the moment the player dies until they are back: applies
//! both parts of the fix each tick, then writes the log summary and picks the
//! on-screen message.

use std::time::{Duration, Instant};

use crate::banner::Message;
use crate::config::Settings;
use crate::game::{Game, Weather, weather_str};
use crate::geom::BLOCK_FADE_INSTANT;
use crate::log::Log;
use crate::mem::{read, write};
use crate::patch::PatchSite;

/// A normal respawn takes 4-7 s from the death; a respawn blocked by a weather
/// transition takes 15 s or more. Respawns slower than this are flagged.
pub const LONG_RESPAWN: Duration = Duration::from_secs(10);

/// Everything we track for one death.
pub struct Death {
    since: Instant,
    /// When the player came back (set when the respawn ends).
    pub ended: Option<Instant>,
    /// The weather check was skipped at some point during this respawn.
    patched: bool,
    /// Scenery fades the fix finished early.
    fades_finished: u32,
    /// Blocks (id, address) whose instant flag the fix set; cleared again
    /// after the respawn.
    flagged_blocks: Vec<(u32, usize)>,
    /// Blocks seen fading on the previous tick. A block is only written once
    /// it is seen fading on two ticks in a row, so a stray read while the game
    /// edits the block map never leads to a write.
    fading_before: Vec<(u32, usize)>,
    /// Weather when the player died, and the latest seen since.
    weather_at_death: Option<Weather>,
    weather_now: Option<Weather>,
    /// Seconds after the death when a weather transition was first seen.
    transition_at: Option<f32>,
    /// Ids of the blocks already reported as fading.
    fades_seen: Vec<u32>,
    /// Verbose details, written only if this respawn turns out interesting
    /// (a weather transition or a long black screen).
    details: Vec<String>,
}

impl Death {
    pub fn new(game: &Game) -> Self {
        let weather = game.weather();
        let mut death = Self {
            since: Instant::now(),
            ended: None,
            patched: false,
            fades_finished: 0,
            flagged_blocks: Vec::new(),
            fading_before: Vec::new(),
            weather_at_death: weather,
            weather_now: weather,
            transition_at: None,
            fades_seen: Vec::new(),
            details: Vec::new(),
        };
        death.note(format!("died at {}", game.player_location()));
        death.note(format!("in-game clock {}", game.clock()));
        death.note(format!("weather {}", weather_str(weather)));
        death
    }

    fn secs(&self) -> f32 {
        self.since.elapsed().as_secs_f32()
    }

    /// Adds a verbose detail line, stamped with the time since the death.
    fn note(&mut self, msg: String) {
        let line = format!("  +{:5.2} s  {msg}", self.secs());
        self.details.push(line);
    }

    /// Called every tick while the respawn is pending.
    pub fn update(&mut self, game: &Game, settings: &Settings, site: &mut PatchSite) {
        // Weather: note the start of a transition.
        let weather = game.weather();
        if let Some(w) = weather {
            if w.in_transition && self.transition_at.is_none() {
                self.transition_at = Some(self.secs());
                self.note(format!(
                    "weather transition started: {} -> {}, respawn point {}",
                    weather_str(self.weather_at_death),
                    weather_str(weather),
                    game.respawn_point()
                ));
            }
        }
        self.weather_now = weather;

        // Test only (dev builds): act as if the fix ran but leave the game
        // alone, to check the "not avoided" message.
        if settings.enabled && settings.skip_fix() {
            self.patched = true;
        }

        // Part 1: skip the weather check.
        if settings.fix_writes() {
            site.enable();
            self.patched |= site.active;
        }

        // Part 2: finish scenery fades (and report every block seen fading).
        let fading = game.fading_blocks();
        for f in &fading {
            if !self.fading_before.contains(&(f.id, f.block)) {
                continue; // first sighting, confirmed (or not) on the next tick
            }
            let finish = settings.fix_writes() && !f.instant;
            let finished = finish && write::<u8>(f.block + BLOCK_FADE_INSTANT, 1);
            if finished {
                self.fades_finished += 1;
                self.flagged_blocks.push((f.id, f.block));
            }
            if !self.fades_seen.contains(&f.id) {
                self.fades_seen.push(f.id);
                let what = match (f.instant, finished, settings.enabled) {
                    (true, _, _) => "instant flag already set by the game",
                    (false, true, _) => "finished by the fix",
                    (false, false, false) => "left to run, fix disabled (15 s)",
                    (false, false, true) => "could not set the instant flag (15 s)",
                };
                self.note(format!("scenery block {:#x} fading: {what}", f.id));
            }
        }
        self.fading_before = fading.into_iter().map(|f| (f.id, f.block)).collect();
    }

    /// Clears the instant flags the fix set. The game does not clear the flag
    /// itself, so without this every later weather change of those blocks
    /// would also be instant, outside of respawns.
    ///
    /// A block may have been unloaded since we set its flag (for example when
    /// the expedition ends), and its memory reused by something else. So the
    /// flag is only cleared on blocks that are still in the block map, under
    /// the same id, with the flag still set.
    pub fn clear_flags(&mut self, game: &Game) {
        if self.flagged_blocks.is_empty() {
            return;
        }
        let live = game.geometry_blocks();
        for (id, block) in self.flagged_blocks.drain(..) {
            if live.contains(&(id, block)) && read::<u8>(block + BLOCK_FADE_INSTANT) == Some(1) {
                write::<u8>(block + BLOCK_FADE_INSTANT, 0);
            }
        }
    }

    /// The respawn is over: one summary line, plus the details if verbose and
    /// the respawn started a weather transition or took long. Returns the
    /// on-screen message for this respawn (see `message`).
    pub fn report(mut self, game: &Game, settings: &Settings, log: &mut Log) -> Option<Message> {
        let ended = self.ended.unwrap_or_else(Instant::now);
        let elapsed = ended - self.since;
        let long = elapsed >= LONG_RESPAWN;
        let transition = match self.transition_at {
            Some(_) => format!(
                ", weather transition {} -> {}",
                weather_str(self.weather_at_death),
                weather_str(self.weather_now)
            ),
            None => String::new(),
        };
        log.line(&format!(
            "respawned {:.2} s after death{}{transition} (fix {}, {} scenery fades finished)",
            elapsed.as_secs_f32(),
            if long { " LONG BLACK SCREEN" } else { "" },
            if self.patched { "used" } else { "off" },
            self.fades_finished
        ));
        if settings.verbose && (long || self.transition_at.is_some()) {
            self.details.push(format!(
                "  +{:5.2} s  back at {}",
                elapsed.as_secs_f32(),
                game.player_location()
            ));
            for line in &self.details {
                log.line(line);
            }
        }
        let weather_wait = self.transition_at.is_some() || !self.fades_seen.is_empty();
        message(self.patched, weather_wait, long)
    }
}

/// The on-screen message after a respawn. Only respawns the fix had to act
/// on get one: the fix was used and a weather wait was seen (a transition
/// while the respawn was pending, or a scenery fade). "Avoided" if the
/// respawn was then short, "not avoided" if it still took long. An ordinary
/// respawn (no weather change) gets none, short or long.
pub fn message(patched: bool, weather_wait: bool, long: bool) -> Option<Message> {
    match (patched && weather_wait, long) {
        (true, false) => Some(Message::Avoided),
        (true, true) => Some(Message::NotAvoided),
        (false, _) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_only_when_the_fix_acted() {
        assert_eq!(message(true, true, false), Some(Message::Avoided));
        assert_eq!(message(true, true, true), Some(Message::NotAvoided));
        assert_eq!(message(true, false, false), None, "ordinary respawn");
        assert_eq!(message(true, false, true), None, "ordinary long respawn");
        assert_eq!(message(false, true, true), None, "fix off");
        assert_eq!(message(false, false, false), None);
    }
}

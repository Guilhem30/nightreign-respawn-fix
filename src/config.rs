//! `respawn_gate_fix.ini`: created with defaults on first run, re-read
//! whenever the file changes.

use std::path::PathBuf;
use std::time::SystemTime;

#[cfg(feature = "dev")]
use crate::banner::Message;

/// The ini file written on first run.
pub const DEFAULT_INI: &str = "\
# Respawn Gate Fix settings. Changes apply while the game runs.

# Turn the whole fix on or off.
enabled = true

# Log details (place, weather, in-game clock) for respawns that start a weather
# transition or take long. Useful for bug reports.
verbose = true

# Show an on-screen message after a respawn the fix shortened, or one that
# still took long.
notify = true
";

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Settings {
    /// The whole fix on or off.
    pub enabled: bool,
    /// Detailed log entries for respawns with a weather transition or a long
    /// black screen.
    pub verbose: bool,
    /// On-screen message after a respawn the fix had to act on.
    pub notify: bool,
    /// Keys for testing, only in builds with the `dev` feature.
    #[cfg(feature = "dev")]
    pub dev: DevSettings,
}

/// Test-only keys (`dev` feature; not in the default ini).
#[cfg(feature = "dev")]
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct DevSettings {
    /// `banner_test = <row>` shows text effect row `<row>`, `<row>t` /
    /// `<row>f` the same row with our "avoided" / "not avoided" text.
    pub banner_test: Option<(u32, Option<Message>)>,
    /// `test_skip_fix = true` makes the fix count as used without changing
    /// anything, to simulate a fix that does not work.
    pub skip_fix: bool,
    /// `chaos = read_fail,garbage,flap,write_fail,banner_panic,banner_layout,
    /// banner_slow`: fault injection for the stress test (see `chaos.rs`).
    pub chaos_modes: u32,
    /// `chaos_rate`: faults per 1000 accesses.
    pub chaos_rate: u32,
    /// `chaos_seed`: makes a run repeatable (0 = default seed).
    pub chaos_seed: u64,
    /// `chaos_count`: how many reads the targeted faults block (default 2).
    pub chaos_count: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            verbose: true,
            notify: true,
            #[cfg(feature = "dev")]
            dev: DevSettings::default(),
        }
    }
}

impl Settings {
    /// Parses `key = value` lines; unknown keys and `#` comments are ignored,
    /// missing keys keep their default. Any value but `false` or `0` is on.
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            let on = !(value.eq_ignore_ascii_case("false") || value == "0");
            match key.trim().to_ascii_lowercase().as_str() {
                "enabled" => s.enabled = on,
                "verbose" => s.verbose = on,
                "notify" => s.notify = on,
                #[cfg(feature = "dev")]
                "test_skip_fix" => s.dev.skip_fix = on,
                #[cfg(feature = "dev")]
                "banner_test" => s.dev.banner_test = parse_banner_test(value),
                #[cfg(feature = "dev")]
                "chaos" => s.dev.chaos_modes = crate::chaos::parse_modes(value),
                #[cfg(feature = "dev")]
                "chaos_rate" => s.dev.chaos_rate = value.parse().unwrap_or(0),
                #[cfg(feature = "dev")]
                "chaos_seed" => s.dev.chaos_seed = value.parse().unwrap_or(0),
                #[cfg(feature = "dev")]
                "chaos_count" => s.dev.chaos_count = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        s
    }

    #[cfg(feature = "dev")]
    fn describe_chaos(&self) -> String {
        if self.dev.chaos_modes == 0 {
            return String::new();
        }
        format!(
            " TEST: chaos modes={:#x} rate={}/1000 seed={}",
            self.dev.chaos_modes, self.dev.chaos_rate, self.dev.chaos_seed
        )
    }

    #[cfg(not(feature = "dev"))]
    fn describe_chaos(&self) -> String {
        String::new()
    }

    /// True if the fix should change the game: enabled, and not simulating a
    /// failing fix (`test_skip_fix`, dev builds only).
    pub fn fix_writes(&self) -> bool {
        self.enabled && !self.skip_fix()
    }

    /// `test_skip_fix` (dev builds only): count the fix as used, change
    /// nothing.
    pub fn skip_fix(&self) -> bool {
        #[cfg(feature = "dev")]
        return self.dev.skip_fix;
        #[cfg(not(feature = "dev"))]
        false
    }

    pub fn describe(&self) -> String {
        format!(
            "enabled={} verbose={} notify={}{}",
            self.enabled,
            self.verbose,
            self.notify,
            if self.skip_fix() {
                " TEST: fix skipped"
            } else {
                ""
            },
        ) + &self.describe_chaos()
    }
}

/// `<row>`, `<row>t` or `<row>f`.
#[cfg(feature = "dev")]
fn parse_banner_test(value: &str) -> Option<(u32, Option<Message>)> {
    let text = match value.chars().last() {
        Some('t') => Some(Message::Avoided),
        Some('f') => Some(Message::NotAvoided),
        _ => None,
    };
    value
        .trim_end_matches(['t', 'f'])
        .parse()
        .ok()
        .map(|row| (row, text))
}

/// The ini file and the settings last read from it.
pub struct Config {
    path: PathBuf,
    modified: Option<SystemTime>,
    pub settings: Settings,
}

impl Config {
    /// Opens the ini, writing the default one if there is none.
    pub fn open(path: PathBuf) -> Self {
        if !path.exists() {
            let _ = std::fs::write(&path, DEFAULT_INI);
        }
        let mut config = Self {
            path,
            modified: None,
            settings: Settings::default(),
        };
        config.refresh();
        config
    }

    /// Re-reads the file if it changed. Returns true if a setting changed.
    pub fn refresh(&mut self) -> bool {
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if modified == self.modified {
            return false;
        }
        self.modified = modified;
        let settings = Settings::parse(&std::fs::read_to_string(&self.path).unwrap_or_default());
        let changed = settings != self.settings;
        self.settings = settings;
        #[cfg(feature = "dev")]
        crate::chaos::configure(
            settings.dev.chaos_modes,
            settings.dev.chaos_rate,
            settings.dev.chaos_seed,
        );
        #[cfg(feature = "dev")]
        crate::chaos::set_count(settings.dev.chaos_count);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ini_gives_defaults() {
        assert_eq!(Settings::parse(DEFAULT_INI), Settings::default());
    }

    #[test]
    fn empty_or_missing_keys_keep_defaults() {
        assert_eq!(Settings::parse(""), Settings::default());
        assert_eq!(
            Settings::parse("# enabled = false\nunknown = 0"),
            Settings::default()
        );
    }

    #[test]
    fn values_and_case() {
        let s = Settings::parse("Enabled = FALSE\n verbose=0 \nnotify = no");
        assert!(!s.enabled);
        assert!(!s.verbose);
        assert!(s.notify, "only false and 0 turn a key off");
        assert!(!s.fix_writes());
    }

    #[cfg(not(feature = "dev"))]
    #[test]
    fn test_keys_ignored_in_release() {
        let s = Settings::parse("test_skip_fix = true\nbanner_test = 4200t");
        assert_eq!(s, Settings::default());
        assert!(s.fix_writes());
    }

    #[cfg(feature = "dev")]
    #[test]
    fn test_keys_in_dev_builds() {
        let s = Settings::parse("test_skip_fix = true\nbanner_test = 4200t");
        assert!(s.skip_fix());
        assert!(!s.fix_writes());
        assert_eq!(s.dev.banner_test, Some((4200, Some(Message::Avoided))));
        assert_eq!(parse_banner_test("4201"), Some((4201, None)));
        assert_eq!(
            parse_banner_test("4200f"),
            Some((4200, Some(Message::NotAvoided)))
        );
        assert_eq!(parse_banner_test("x"), None);
    }

    #[test]
    fn random_text_never_panics() {
        use crate::testutil::Rng;
        let tokens = [
            "enabled",
            "verbose",
            "notify",
            "=",
            "\n",
            " ",
            "true",
            "false",
            "0",
            "#",
            "banner_test",
            "4200t",
            "99999999999",
            "chaos",
            "chaos_rate",
            "-1",
            "\u{fc}",
            "\0",
            "test_skip_fix",
            "\r\n",
            "====",
        ];
        for seed in 1..=500u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let text: String = (0..rng.below(60))
                .map(|_| tokens[rng.below(tokens.len() as u64) as usize])
                .collect();
            let _ = Settings::parse(&text);
            let bytes: Vec<u8> = (0..rng.below(300)).map(|_| rng.next() as u8).collect();
            let _ = Settings::parse(&String::from_utf8_lossy(&bytes));
        }
        // A huge line is fine too.
        let _ = Settings::parse(&"x=".repeat(100_000));
    }
}

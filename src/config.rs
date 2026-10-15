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
                _ => {}
            }
        }
        s
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
            }
        )
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
}

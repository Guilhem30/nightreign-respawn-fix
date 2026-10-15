//! Finding game code by byte pattern, so no address is hard-coded.
//!
//! Patterns are written as text, `"48 8B 0D ?? ?? ?? ??"`, and turned into
//! `[Option<u8>]` at compile time by `pattern!` (`None` = any byte). A typo in
//! a pattern is a build error.

use pelite::pe64::{Pe, PeView};

use crate::mem::{exe_base, read_bytes, rel32_target};

/// A code pattern: `None` matches any byte.
pub type Pattern = [Option<u8>];

/// Builds a `&'static Pattern` from a string of hex bytes and `??`.
macro_rules! pattern {
    ($text:literal) => {{
        const BYTES: [Option<u8>; $crate::scan::token_count($text)] = $crate::scan::parse($text);
        &BYTES
    }};
}
pub(crate) use pattern;

/// Number of space-separated tokens in a pattern string.
pub const fn token_count(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0;
    let mut i = 0;
    let mut in_token = false;
    while i < bytes.len() {
        let space = bytes[i] == b' ';
        if !space && !in_token {
            count += 1;
        }
        in_token = !space;
        i += 1;
    }
    count
}

const fn hex_digit(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'A'..=b'F' => c - b'A' + 10,
        b'a'..=b'f' => c - b'a' + 10,
        _ => panic!("pattern: not a hex digit"),
    }
}

/// Parses a pattern string: two hex digits per byte, `??` for any byte, one
/// space between tokens.
pub const fn parse<const N: usize>(text: &str) -> [Option<u8>; N] {
    let bytes = text.as_bytes();
    let mut out = [None; N];
    let mut i = 0;
    let mut n = 0;
    while i < bytes.len() {
        if bytes[i] == b' ' {
            i += 1;
            continue;
        }
        assert!(i + 1 < bytes.len(), "pattern: token of one character");
        assert!(
            i + 2 == bytes.len() || bytes[i + 2] == b' ',
            "pattern: token longer than two characters"
        );
        out[n] = if bytes[i] == b'?' && bytes[i + 1] == b'?' {
            None
        } else {
            Some(hex_digit(bytes[i]) << 4 | hex_digit(bytes[i + 1]))
        };
        n += 1;
        i += 2;
    }
    assert!(n == N, "pattern: wrong token count");
    out
}

/// True if `pattern` holds exactly `bytes` (no wildcards) at `at`. For
/// compile-time checks that a patch offset points at the expected code.
pub const fn has_bytes(pattern: &Pattern, at: usize, bytes: &[u8]) -> bool {
    if at + bytes.len() > pattern.len() {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        match pattern[at + i] {
            Some(b) if b == bytes[i] => {}
            _ => return false,
        }
        i += 1;
    }
    true
}

/// Offsets in `code` where `pattern` matches.
pub fn match_offsets(code: &[u8], pattern: &Pattern) -> Vec<usize> {
    if pattern.is_empty() {
        return Vec::new();
    }
    code.windows(pattern.len())
        .enumerate()
        .filter(|(_, window)| {
            window
                .iter()
                .zip(pattern)
                .all(|(byte, want)| want.is_none_or(|w| w == *byte))
        })
        .map(|(offset, _)| offset)
        .collect()
}

/// Searches the game's `.text` section for a pattern. Returns the address of
/// every match.
pub fn find_all(pattern: &Pattern) -> Result<Vec<usize>, String> {
    let base = exe_base();
    // SAFETY: `base` is the game exe, mapped for the life of the process.
    let view = unsafe { PeView::module(base as *const u8) };
    let text = view
        .section_headers()
        .iter()
        .find(|s| s.name_bytes() == b".text")
        .ok_or("no .text section in the game exe")?;
    let start = base + text.VirtualAddress as usize;
    let code = read_bytes(start, text.VirtualSize as usize).ok_or("cannot read .text")?;
    Ok(match_offsets(&code, pattern)
        .into_iter()
        .map(|offset| start + offset)
        .collect())
}

/// Like `find_all`, but the pattern must match exactly once.
pub fn find_unique(pattern: &Pattern, what: &str) -> Result<usize, String> {
    match find_all(pattern)?.as_slice() {
        [site] => Ok(*site),
        [] => Err(format!("{what}: code pattern not found (game updated?)")),
        matches => Err(format!(
            "{what}: code pattern found {} times, expected once",
            matches.len()
        )),
    }
}

/// Like `find_unique`, but accepts several matches if they all call the
/// same function (`rel32` call at `call_at`). Returns that function.
pub fn find_unique_or_same(pattern: &Pattern, call_at: usize, what: &str) -> Result<usize, String> {
    let targets: Vec<usize> = find_all(pattern)?
        .into_iter()
        .filter_map(|m| rel32_target(m + call_at))
        .collect();
    match targets.split_first() {
        Some((first, rest)) if rest.iter().all(|t| t == first) => Ok(*first),
        Some(_) => Err(format!("{what}: matches call different functions")),
        None => Err(format!("{what}: pattern not found (game updated?)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bytes_and_wildcards() {
        let p: &Pattern = pattern!("48 8b ?? E8");
        assert_eq!(p, [Some(0x48), Some(0x8B), None, Some(0xE8)]);
        assert_eq!(token_count("  48  ?? "), 2);
    }

    #[test]
    fn checks_fixed_bytes() {
        let p: &Pattern = pattern!("74 ?? 33 D2");
        assert!(has_bytes(p, 2, &[0x33, 0xD2]));
        assert!(!has_bytes(p, 1, &[0x00]), "a wildcard is not a fixed byte");
        assert!(!has_bytes(p, 3, &[0xD2, 0x00]), "past the end");
    }

    #[test]
    fn matches_with_wildcards() {
        let code = [0x00, 0x74, 0x05, 0x33, 0xD2, 0x74, 0x10, 0x33, 0xD2, 0x33];
        assert_eq!(match_offsets(&code, pattern!("74 ?? 33 D2")), [1, 5]);
        assert_eq!(match_offsets(&code, pattern!("33 D2 33")), [7]);
        assert!(match_offsets(&code, pattern!("33 C0")).is_empty());
    }

    #[test]
    fn pattern_longer_than_code() {
        assert!(match_offsets(&[0x33], pattern!("33 D2")).is_empty());
        assert!(match_offsets(&[], pattern!("33")).is_empty());
    }
}

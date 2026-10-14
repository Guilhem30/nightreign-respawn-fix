//! # Respawn Gate Fix for Elden Ring Nightreign
//!
//! Fixes the long (~15 s extra) black screen after dying when the respawn
//! point is in an area with a different weather than the place you died
//! (most visible during the Night's Tide, when the rain kills you and you
//! respawn far away).
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
//! 1. **Weather check skipped.** The "is stable" function takes a second
//!    argument; when it is non-zero the weather check is skipped (other
//!    callers in the game use this). The respawn condition always passes 0:
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
//! 2. **Scenery fade finished.** Each geometry block has an "instant" flag
//!    that the game checks during a fade: when it is set, the fade jumps to
//!    its end on the next frame. The DLL sets that flag on every block that is
//!    fading, and clears it again once the player is back (the game leaves
//!    the flag set, which would make later weather changes instant too).
//!    Sometimes the flag is already set; then there is nothing to do. This part is required: the geometry check waits for the fade,
//!    so skipping only the weather check still leaves a 15 s black screen.
//!
//! Map, collision, characters and the rest of the geometry still have to be
//! ready before the respawn, so you never respawn into unloaded terrain. The
//! lighting blend finishes after the screen fades in instead of before.
//!
//! ## Files (next to the DLL)
//!
//! * `respawn_gate_fix.ini`: settings, created with defaults on first run and
//!   re-read while the game runs:
//!   * `enabled = true`: the whole fix on or off,
//!   * `verbose = true`: detailed log entries (see below).
//! * `respawn_gate_fix.log`: one line per death with the time until the
//!   player came back. With `verbose`, a respawn that started a weather
//!   transition or took long also gets the details: where the player died,
//!   the respawn point, the weather before and after, the in-game clock, and
//!   what happened to the scenery fade. This beta collects that information to
//!   find the cases the fix does not cover yet.
//!
//! ## Safety notes
//!
//! * All work runs on one background thread of our own. Game data is read
//!   with `ReadProcessMemory`, so a stale pointer fails cleanly instead of
//!   crashing.
//! * Game memory is written only while a respawn is pending, and only the
//!   2-byte instruction and the one-byte fade flag described above; both are
//!   restored when the player is back.
//! * The code location is found with a byte pattern, not a fixed address, and
//!   must match exactly once. If a game update changes that code, the DLL logs
//!   it and does nothing.
//! * Each patch is one 2-byte store, so the game thread sees either the old or
//!   the new instruction, never half of each.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use pelite::pe64::{Pe, PeView};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::{
    FlushInstructionCache, ReadProcessMemory, WriteProcessMemory,
};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Memory::{
    PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, VirtualProtect,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::GetCurrentProcess;

// ---------------------------------------------------------------- constants

/// Mod name and version (from Cargo.toml), written at the top of each session
/// in the log.
const NAME: &str = "Respawn Gate Fix";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The respawn condition's call to `FieldArea::IsStable`, as a byte pattern.
/// `None` is a wildcard (addresses and jump distances that change between
/// game builds).
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
#[rustfmt::skip]
const PATTERN: &[Option<u8>] = &[
    Some(0x48), Some(0x8B), Some(0x0D), None, None, None, None,
    Some(0x48), Some(0x85), Some(0xC9),
    Some(0x74), None,
    Some(0x33), Some(0xD2),
    Some(0xE8), None, None, None, None,
    Some(0x84), Some(0xC0),
    Some(0x74), None,
    Some(0x44), Some(0x8B), Some(0x47), Some(0x10),
    Some(0x45), Some(0x33), Some(0xC9),
    Some(0x48), Some(0x8B), Some(0xCB),
    Some(0xC7), Some(0x44), Some(0x24), Some(0x20), Some(0x00), Some(0x00), Some(0x00), Some(0x00),
    Some(0x41), Some(0x8D), Some(0x51), Some(0x48),
];

/// Where the `xor edx, edx` starts inside the pattern.
const PATCH_OFFSET: usize = 12;

/// `xor edx, edx`: second argument = 0, wait for the weather (game default).
const ORIGINAL: [u8; 2] = [0x33, 0xD2];

/// `mov dl, 1`: second argument = 1, skip the weather check.
const PATCHED: [u8; 2] = [0xB2, 0x01];

/// `CSEventMan` -> `+0x10` -> `+0x8` holds 4 from the moment the player dies
/// until they are back in the world.
const EVENT_STATE_RESPAWN_PENDING: u32 = 4;

/// Geometry block fields (`CSWorldGeomMan` blocks; the class has no RTTI name).
/// `+0x218` is the weather fade state (0 = idle, 3 = fading), `+0x21e` the
/// "instant" flag: while it is set, the game ends a fade on the next frame
/// (`fade_time = fade_length`).
const BLOCK_FADE_STATE: usize = 0x218;
const BLOCK_FADE_INSTANT: usize = 0x21e;
const FADE_STATE_FADING: u32 = 3;

/// `WorldAreaWeather` fields: `+0x18` = 1 while a weather transition runs,
/// `+0x28` low 16 bits = the base weather of the area, `+0x30` = an override
/// weather for the area (0 = none). The area's weather is the override if set,
/// else the base.
const WEATHER_IN_TRANSITION: usize = 0x18;
const WEATHER_BASE: usize = 0x28;
const WEATHER_OVERRIDE: usize = 0x30;

/// `WorldPlayAreaManager` (the Night's Tide): `+0x100` packed in-game clock,
/// `+0x260` circle radius (m).
const PLAY_AREA_CLOCK: usize = 0x100;
const PLAY_AREA_RADIUS: usize = 0x260;

/// A normal respawn takes 4-7 s from the death; a respawn blocked by a weather
/// transition takes 15 s or more. Respawns slower than this are flagged.
const LONG_RESPAWN: Duration = Duration::from_secs(10);

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

// ---------------------------------------------------------------- memory

fn exe_base() -> usize {
    unsafe { GetModuleHandleW(None) }.map(|h| h.0 as usize).unwrap_or(0)
}

/// Reads a value from game memory. Returns `None` instead of crashing if the
/// address is not readable.
fn read<T: Copy + Default>(addr: usize) -> Option<T> {
    if addr == 0 {
        return None;
    }
    let mut value = T::default();
    let mut done = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            addr as *const c_void,
            &mut value as *mut T as *mut c_void,
            size_of::<T>(),
            Some(&mut done),
        )
    };
    (ok.is_ok() && done == size_of::<T>()).then_some(value)
}

/// Reads a pointer; 0 if unreadable.
fn read_ptr(addr: usize) -> usize {
    read::<usize>(addr).unwrap_or(0)
}

/// Writes a value to game memory; false if the address is not writable.
fn write<T: Copy>(addr: usize, value: T) -> bool {
    let mut done = 0usize;
    let ok = unsafe {
        WriteProcessMemory(
            GetCurrentProcess(),
            addr as *const c_void,
            &value as *const T as *const c_void,
            size_of::<T>(),
            Some(&mut done),
        )
    };
    ok.is_ok() && done == size_of::<T>()
}

/// Copies `len` bytes of game memory into a buffer.
fn read_bytes(addr: usize, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut done = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            addr as *const c_void,
            buf.as_mut_ptr() as *mut c_void,
            len,
            Some(&mut done),
        )
    };
    (ok.is_ok() && done == len).then_some(buf)
}

// ---------------------------------------------------------------- finding the code

/// Searches the game's `.text` section for `PATTERN`. Returns the address of
/// the instruction to patch, or an error message if it matched zero or several
/// times.
fn find_patch_site() -> Result<usize, String> {
    let base = exe_base();
    let view = unsafe { PeView::module(base as *const u8) };
    let text = view
        .section_headers()
        .iter()
        .find(|s| s.name_bytes() == b".text")
        .ok_or("no .text section in the game exe")?;
    let start = base + text.VirtualAddress as usize;
    let code = read_bytes(start, text.VirtualSize as usize).ok_or("cannot read .text")?;

    let matches: Vec<usize> = code
        .windows(PATTERN.len())
        .enumerate()
        .filter(|(_, window)| {
            window
                .iter()
                .zip(PATTERN)
                .all(|(byte, want)| want.is_none_or(|w| w == *byte))
        })
        .map(|(offset, _)| start + offset + PATCH_OFFSET)
        .collect();

    match matches.as_slice() {
        [site] => Ok(*site),
        [] => Err("code pattern not found (game updated?)".into()),
        _ => Err(format!("code pattern found {} times, expected once", matches.len())),
    }
}

// ---------------------------------------------------------------- patching

/// The 2-byte instruction we switch between `ORIGINAL` and `PATCHED`.
struct PatchSite {
    addr: usize,
    active: bool,
}

impl PatchSite {
    /// Writes `bytes` over the instruction as a single 16-bit store.
    ///
    /// The main thread may be running this exact code at the same moment.
    /// A 16-bit store that does not cross a 64-byte cache line is atomic on
    /// x86-64, so the game sees either the old or the new instruction.
    /// (A plain `memcpy` could write byte by byte, and a half-written
    /// `33 01` would decode as a different instruction.)
    fn write(&mut self, bytes: [u8; 2]) -> bool {
        let value = u16::from_le_bytes(bytes);
        let mut old = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            if VirtualProtect(self.addr as *const c_void, 2, PAGE_EXECUTE_READWRITE, &mut old)
                .is_err()
            {
                return false;
            }
            // One `mov word ptr [addr], value` instruction.
            std::arch::asm!(
                "mov word ptr [{addr}], {value:x}",
                addr = in(reg) self.addr,
                value = in(reg) value,
                options(nostack, preserves_flags),
            );
            let mut ignored = PAGE_PROTECTION_FLAGS(0);
            let _ = VirtualProtect(self.addr as *const c_void, 2, old, &mut ignored);
            let _ = FlushInstructionCache(GetCurrentProcess(), Some(self.addr as *const c_void), 2);
        }
        read::<[u8; 2]>(self.addr) == Some(bytes)
    }

    /// Skips the weather check (call while a respawn is pending).
    fn enable(&mut self) {
        if !self.active && self.write(PATCHED) {
            self.active = true;
        }
    }

    /// Restores the game's original code.
    fn disable(&mut self) {
        if self.active && self.write(ORIGINAL) {
            self.active = false;
        }
    }
}

// ---------------------------------------------------------------- log and settings

/// Appends timestamped lines to `respawn_gate_fix.log`.
struct Log(Option<File>);

impl Log {
    fn line(&mut self, msg: &str) {
        let t = unsafe { GetLocalTime() };
        if let Some(f) = &mut self.0 {
            let _ = writeln!(
                f,
                "{:02}:{:02}:{:02}.{:03} {msg}",
                t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
            );
            let _ = f.flush();
        }
    }
}

/// The ini file written on first run.
const DEFAULT_INI: &str = "\
# Respawn Gate Fix settings. Changes apply while the game runs.

# Turn the whole fix on or off.
enabled = true

# Log details (place, weather, in-game clock) for respawns that start a weather
# transition or take long. Useful for bug reports.
verbose = true
";

/// `respawn_gate_fix.ini`, re-read whenever the file changes.
#[derive(Clone, Copy, PartialEq)]
struct Settings {
    enabled: bool,
    verbose: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            verbose: true,
        }
    }
}

impl Settings {
    /// Parses `key = value` lines; unknown keys and `#` comments are ignored,
    /// missing keys keep their default.
    fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            let on = !(value.eq_ignore_ascii_case("false") || value == "0");
            match key.trim().to_ascii_lowercase().as_str() {
                "enabled" => s.enabled = on,
                "verbose" => s.verbose = on,
                _ => {}
            }
        }
        s
    }

    fn describe(&self) -> String {
        format!(
            "enabled={} verbose={}",
            self.enabled, self.verbose
        )
    }
}

struct Config {
    path: PathBuf,
    modified: Option<SystemTime>,
    settings: Settings,
}

impl Config {
    fn open(path: PathBuf) -> Self {
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
    fn refresh(&mut self) -> bool {
        let modified = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
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

// ---------------------------------------------------------------- game state

/// Static pointers to the game singletons we use (0 if not found).
struct Game {
    /// `CSEventMan`: tells whether a respawn is pending.
    event_man: usize,
    /// `CSWorldGeomMan`: owns the geometry blocks.
    geom_man: usize,
    /// The rest is only used for the verbose log.
    weather: usize,
    world_chr_man: usize,
    respawn_point_man: usize,
    play_area: usize,
}

/// A map position: block id (e.g. m60_44_37_00) and block-local x, y, z.
fn location(block_id: u32, xyz: [f32; 3]) -> String {
    let [x, y, z] = xyz;
    format!(
        "m{}_{:02}_{:02}_{:02x} ({x:.1}, {y:.1}, {z:.1})",
        block_id >> 24,
        block_id >> 16 & 0xff,
        block_id >> 8 & 0xff,
        block_id & 0xff
    )
}

/// The area weather and whether a transition runs.
#[derive(Clone, Copy, PartialEq)]
struct Weather {
    in_transition: bool,
    /// Weather id (WeatherParam row) of the player's area.
    id: u32,
}

impl Game {
    /// Looks the singletons up by name. `None` until the game has registered
    /// the two required ones (early in the boot).
    fn find() -> Option<Self> {
        let view = unsafe { PeView::module(exe_base() as *const u8) };
        let found = from_singleton::find::fd4_singletons(view);
        if found.all_null() {
            return None;
        }
        let map = unsafe { found.finish() };
        let get = |name: &str| map.get(name).map(|p| p.as_ptr() as usize);
        Some(Self {
            event_man: get("CSEventMan")?,
            geom_man: get("CSWorldGeomMan")?,
            weather: get("WorldAreaWeather").unwrap_or(0),
            world_chr_man: get("WorldChrMan").unwrap_or(0),
            respawn_point_man: get("CSRespawnPointMan").unwrap_or(0),
            play_area: get("WorldPlayAreaManager").unwrap_or(0),
        })
    }

    /// True from the player's death until they are back in the world.
    fn respawn_pending(&self) -> bool {
        let event_man = read_ptr(self.event_man);
        let state_obj = read_ptr(event_man + 0x10);
        state_obj != 0 && read::<u32>(state_obj + 0x8) == Some(EVENT_STATE_RESPAWN_PENDING)
    }

    /// All geometry blocks as (block id, block address). `CSWorldGeomMan +
    /// 0x20` is an MSVC `std::map` (red-black tree). Its head node links to the
    /// leftmost node; each node has `+0x0` left, `+0x8` parent, `+0x10` right,
    /// `+0x19` "is nil" flag, `+0x20` the block id and `+0x28` the block. This
    /// walks it in order without recursion.
    fn geometry_blocks(&self) -> Vec<(u32, usize)> {
        let mut blocks = Vec::new();
        let head = read_ptr(read_ptr(self.geom_man) + 0x20);
        if head == 0 {
            return blocks;
        }
        let is_nil = |node: usize| read::<u8>(node + 0x19).unwrap_or(1) != 0;

        let mut node = read_ptr(head); // leftmost node
        while node != 0 && node != head && blocks.len() < 4096 {
            let id = read::<u32>(node + 0x20).unwrap_or(0);
            blocks.push((id, read_ptr(node + 0x28)));

            // Next node in order: leftmost of the right subtree, or else the
            // first ancestor we reach from its left side.
            let right = read_ptr(node + 0x10);
            if right != 0 && !is_nil(right) {
                node = right;
                loop {
                    let left = read_ptr(node);
                    if left == 0 || is_nil(left) {
                        break;
                    }
                    node = left;
                }
            } else {
                let mut parent = read_ptr(node + 0x8);
                while parent != 0 && !is_nil(parent) && node == read_ptr(parent + 0x10) {
                    node = parent;
                    parent = read_ptr(node + 0x8);
                }
                node = parent;
            }
        }
        blocks
    }

    /// Blocks that are fading, as (block id, block address, instant flag).
    fn fading_blocks(&self) -> Vec<(u32, usize, bool)> {
        self.geometry_blocks()
            .into_iter()
            .filter(|(_, block)| read::<u32>(block + BLOCK_FADE_STATE) == Some(FADE_STATE_FADING))
            .map(|(id, block)| {
                let instant = read::<u8>(block + BLOCK_FADE_INSTANT).is_some_and(|f| f != 0);
                (id, block, instant)
            })
            .collect()
    }

    fn weather(&self) -> Option<Weather> {
        let w = read_ptr(self.weather);
        let base = read::<u32>(w + WEATHER_BASE)? & 0xffff;
        let over = read::<u32>(w + WEATHER_OVERRIDE)?;
        Some(Weather {
            in_transition: read::<u32>(w + WEATHER_IN_TRANSITION)? == 1,
            id: if over != 0 { over } else { base },
        })
    }

    /// Player position: `WorldChrMan + 0x174e8` -> player, `+0x5f0` -> map
    /// position: `+0x64` block id, `+0x68` x, y, z.
    fn player_location(&self) -> String {
        let player = read_ptr(read_ptr(self.world_chr_man) + 0x174e8);
        let pos = read_ptr(player + 0x5f0);
        match (read::<u32>(pos + 0x64), read::<[f32; 3]>(pos + 0x68)) {
            (Some(id), Some(xyz)) if player != 0 => location(id, xyz),
            _ => "?".into(),
        }
    }

    /// Respawn point chosen by the game: `CSRespawnPointMan + 0x18` block id,
    /// `+0x20` x, y, z.
    fn respawn_point(&self) -> String {
        let m = read_ptr(self.respawn_point_man);
        match (read::<u32>(m + 0x18), read::<[f32; 3]>(m + 0x20)) {
            (Some(0xffff_ffff), _) => "none".into(),
            (Some(id), Some(xyz)) if m != 0 => location(id, xyz),
            _ => "?".into(),
        }
    }

    /// In-game clock and Night's Tide circle radius.
    fn clock(&self) -> String {
        let p = read_ptr(self.play_area);
        match (read::<u64>(p + PLAY_AREA_CLOCK), read::<f32>(p + PLAY_AREA_RADIUS)) {
            (Some(c), Some(r)) if p != 0 => format!(
                "{:02}:{:02}:{:02}, circle radius {r:.0} m",
                c >> 34 & 0x1f,
                c >> 39 & 0x3f,
                c >> 45 & 0x3f
            ),
            _ => "?".into(),
        }
    }
}

// ---------------------------------------------------------------- one death

/// Everything we track for one death, from the moment the player dies until
/// they are back.
struct Death {
    since: Instant,
    /// When the player came back (set when the respawn ends).
    ended: Option<Instant>,
    /// The weather check was skipped at some point during this respawn.
    patched: bool,
    /// Scenery fades the fix finished early.
    fades_finished: u32,
    /// Blocks whose instant flag the fix set; cleared again after the respawn.
    flagged_blocks: Vec<usize>,
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
    fn new(game: &Game) -> Self {
        let weather = game.weather();
        let mut death = Self {
            since: Instant::now(),
            ended: None,
            patched: false,
            fades_finished: 0,
            flagged_blocks: Vec::new(),
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
    fn update(&mut self, game: &Game, settings: &Settings, site: &mut PatchSite) {
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

        // Part 1: skip the weather check.
        if settings.enabled {
            site.enable();
            self.patched |= site.active;
        }

        // Part 2: finish scenery fades (and report every block seen fading).
        for (id, block, instant) in game.fading_blocks() {
            let finish = settings.enabled && !instant;
            let finished = finish && write::<u8>(block + BLOCK_FADE_INSTANT, 1);
            if finished {
                self.fades_finished += 1;
                self.flagged_blocks.push(block);
            }
            if !self.fades_seen.contains(&id) {
                self.fades_seen.push(id);
                let what = match (instant, finished, settings.enabled) {
                    (true, _, _) => "instant flag already set by the game",
                    (false, true, _) => "finished by the fix",
                    (false, false, false) => "left to run, fix disabled (15 s)",
                    (false, false, true) => "could not set the instant flag (15 s)",
                };
                self.note(format!("scenery block {id:#x} fading: {what}"));
            }
        }
    }

    /// Clears the instant flags the fix set. The game does not clear the flag
    /// itself, so without this every later weather change of those blocks
    /// would also be instant, outside of respawns.
    fn clear_flags(&mut self) {
        for block in self.flagged_blocks.drain(..) {
            write::<u8>(block + BLOCK_FADE_INSTANT, 0);
        }
    }

    /// The respawn is over: one summary line, plus the details if verbose and
    /// the respawn started a weather transition or took long.
    fn report(mut self, game: &Game, settings: &Settings, log: &mut Log) {
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
    }
}

fn weather_str(w: Option<Weather>) -> String {
    match w {
        Some(w) => format!("{:#x}", w.id),
        None => "?".into(),
    }
}

// ---------------------------------------------------------------- main loop

fn run(dir: &Path) {
    let mut log = Log(OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("respawn_gate_fix.log"))
        .ok());
    let mut config = Config::open(dir.join("respawn_gate_fix.ini"));
    log.line(&format!(
        "{NAME} {VERSION} loaded (pid {}), {}",
        std::process::id(),
        config.settings.describe()
    ));

    let addr = match find_patch_site() {
        Ok(addr) => addr,
        Err(e) => {
            log.line(&format!("not active: {e}"));
            return;
        }
    };
    if addr % 64 == 63 {
        log.line("not active: the instruction crosses a cache line, cannot patch it atomically");
        return;
    }
    log.line(&format!("respawn condition found at exe+{:#x}", addr - exe_base()));
    let mut site = PatchSite { addr, active: false };

    // The singletons are registered while the game boots.
    let game = loop {
        if let Some(game) = Game::find() {
            break game;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    log.line("ready");

    let mut death: Option<Death> = None;
    let mut last_config_check = Instant::now();
    loop {
        std::thread::sleep(TICK);

        if last_config_check.elapsed() >= CONFIG_CHECK {
            last_config_check = Instant::now();
            if config.refresh() {
                log.line(&format!("settings changed: {}", config.settings.describe()));
            }
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
                d.clear_flags();
                d.ended = Some(Instant::now());
                death = Some(d);
            }
            // Report once the delay is over (or at once if the player died
            // again in the meantime).
            (Some(d), pending) => {
                let ended = d.ended.unwrap_or_else(Instant::now);
                if pending || ended.elapsed() >= REPORT_DELAY {
                    d.report(&game, &settings, &mut log);
                } else {
                    death = Some(d);
                }
            }
            (None, false) => {}
        }
    }
}

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
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    let dir = PathBuf::from(String::from_utf16_lossy(&buf[..len]))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::thread::Builder::new()
        .name("respawn-gate-fix".into())
        .spawn(move || run(&dir))
        .ok();
    true
}

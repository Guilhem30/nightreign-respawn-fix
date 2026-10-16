//! The game state the fix reads: the respawn state, the area weather and,
//! for the verbose log, places and the in-game clock.

use pelite::pe64::PeView;

use crate::geom;
use crate::mem::{Process, exe_base, read, read_ptr};

/// `CSEventMan` -> `+0x10` -> `+0x8` holds 4 from the moment the player dies
/// until they are back in the world.
const EVENT_STATE: usize = 0x10;
const EVENT_STATE_VALUE: usize = 0x8;
const EVENT_STATE_RESPAWN_PENDING: u32 = 4;

/// `WorldAreaWeather` fields: `+0x18` = 1 while a weather transition runs,
/// `+0x28` low 16 bits = the base weather of the area, `+0x30` = an override
/// weather for the area (0 = none). The area's weather is the override if set,
/// else the base.
const WEATHER_IN_TRANSITION: usize = 0x18;
const WEATHER_BASE: usize = 0x28;
const WEATHER_OVERRIDE: usize = 0x30;

/// `WorldChrMan + 0x174e8` -> player, `+0x5f0` -> map position: `+0x64`
/// block id, `+0x68` x, y, z.
const CHR_MAN_PLAYER: usize = 0x174e8;
const PLAYER_MAP_POS: usize = 0x5f0;
const MAP_POS_BLOCK: usize = 0x64;
const MAP_POS_XYZ: usize = 0x68;

/// `CSRespawnPointMan + 0x18` block id (`0xffffffff` = none), `+0x20` x, y, z.
const RESPAWN_BLOCK: usize = 0x18;
const RESPAWN_XYZ: usize = 0x20;

/// `WorldPlayAreaManager` (the Night's Tide): `+0x100` packed in-game clock,
/// `+0x260` circle radius (m).
const PLAY_AREA_CLOCK: usize = 0x100;
const PLAY_AREA_RADIUS: usize = 0x260;

/// Static pointers to the game singletons we use (0 if not found).
pub struct Game {
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

/// The area weather and whether a transition runs.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Weather {
    pub in_transition: bool,
    /// Weather id (WeatherParam row) of the player's area.
    pub id: u32,
}

impl Game {
    /// A `Game` over test structures: `event_man` and `geom_man` are the
    /// addresses of the slots that hold the singleton pointers.
    #[cfg(test)]
    pub fn for_test(event_man: usize, geom_man: usize) -> Self {
        Self {
            event_man,
            geom_man,
            weather: 0,
            world_chr_man: 0,
            respawn_point_man: 0,
            play_area: 0,
        }
    }

    /// Looks the singletons up by name. `None` until the game has registered
    /// the two required ones (early in the boot).
    pub fn find() -> Option<Self> {
        // SAFETY: the game exe, mapped for the life of the process.
        let view = unsafe { PeView::module(exe_base() as *const u8) };
        let found = from_singleton::find::fd4_singletons(view);
        if found.all_null() {
            return None;
        }
        // SAFETY: the game has registered its singletons (checked above).
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
    pub fn respawn_pending(&self) -> bool {
        let state_obj = read_ptr(read_ptr(self.event_man) + EVENT_STATE);
        let pending = state_obj != 0
            && read::<u32>(state_obj + EVENT_STATE_VALUE) == Some(EVENT_STATE_RESPAWN_PENDING);
        // Stress test only: the state flips at random.
        #[cfg(feature = "dev")]
        if crate::chaos::roll(crate::chaos::Fault::Flap) {
            return !pending;
        }
        pending
    }

    /// All geometry blocks as (block id, block address); `None` if the map
    /// cannot be read right now.
    pub fn geometry_blocks(&self) -> Option<Vec<(u32, usize)>> {
        // Stress test only: the block map is unreadable for a few reads.
        #[cfg(feature = "dev")]
        if crate::chaos::map_blocked() {
            return None;
        }
        geom::blocks(&Process, read_ptr(self.geom_man))
    }

    /// Blocks that are fading.
    pub fn fading_blocks(&self) -> Vec<geom::Fading> {
        geom::fading(&Process, read_ptr(self.geom_man))
    }

    pub fn weather(&self) -> Option<Weather> {
        let w = read_ptr(self.weather);
        let base = read::<u32>(w + WEATHER_BASE)? & 0xffff;
        let over = read::<u32>(w + WEATHER_OVERRIDE)?;
        Some(Weather {
            in_transition: read::<u32>(w + WEATHER_IN_TRANSITION)? == 1,
            id: if over != 0 { over } else { base },
        })
    }

    pub fn player_location(&self) -> String {
        let player = read_ptr(read_ptr(self.world_chr_man) + CHR_MAN_PLAYER);
        let pos = read_ptr(player + PLAYER_MAP_POS);
        match (
            read::<u32>(pos + MAP_POS_BLOCK),
            read::<[f32; 3]>(pos + MAP_POS_XYZ),
        ) {
            (Some(id), Some(xyz)) if player != 0 => location(id, xyz),
            _ => "?".into(),
        }
    }

    /// Respawn point chosen by the game.
    pub fn respawn_point(&self) -> String {
        let m = read_ptr(self.respawn_point_man);
        match (
            read::<u32>(m + RESPAWN_BLOCK),
            read::<[f32; 3]>(m + RESPAWN_XYZ),
        ) {
            (Some(0xffff_ffff), _) => "none".into(),
            (Some(id), Some(xyz)) if m != 0 => location(id, xyz),
            _ => "?".into(),
        }
    }

    /// In-game clock and Night's Tide circle radius.
    pub fn clock(&self) -> String {
        let p = read_ptr(self.play_area);
        match (
            read::<u64>(p + PLAY_AREA_CLOCK),
            read::<f32>(p + PLAY_AREA_RADIUS),
        ) {
            (Some(c), Some(r)) if p != 0 => clock(c, r),
            _ => "?".into(),
        }
    }
}

/// A map position: block id (e.g. m60_44_37_00) and block-local x, y, z.
pub fn location(block_id: u32, xyz: [f32; 3]) -> String {
    let [x, y, z] = xyz;
    format!(
        "m{}_{:02}_{:02}_{:02x} ({x:.1}, {y:.1}, {z:.1})",
        block_id >> 24,
        block_id >> 16 & 0xff,
        block_id >> 8 & 0xff,
        block_id & 0xff
    )
}

/// The packed in-game clock (hours at bit 34, minutes at 39, seconds at 45)
/// and the circle radius.
pub fn clock(packed: u64, radius: f32) -> String {
    format!(
        "{:02}:{:02}:{:02}, circle radius {radius:.0} m",
        packed >> 34 & 0x1f,
        packed >> 39 & 0x3f,
        packed >> 45 & 0x3f
    )
}

pub fn weather_str(w: Option<Weather>) -> String {
    match w {
        Some(w) => format!("{:#x}", w.id),
        None => "?".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_location() {
        assert_eq!(
            location(0x3c2c2500, [1.26, -2.0, 300.04]),
            "m60_44_37_00 (1.3, -2.0, 300.0)"
        );
        assert_eq!(
            location(0x0d000a1f, [0.0; 3]),
            "m13_00_10_1f (0.0, 0.0, 0.0)"
        );
    }

    #[test]
    fn decodes_clock() {
        let packed = 13u64 << 34 | 5 << 39 | 59 << 45;
        assert_eq!(clock(packed, 812.4), "13:05:59, circle radius 812 m");
    }

    #[test]
    fn formats_weather() {
        assert_eq!(
            weather_str(Some(Weather {
                in_transition: false,
                id: 0x33
            })),
            "0x33"
        );
        assert_eq!(weather_str(None), "?");
    }
}

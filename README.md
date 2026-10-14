# Respawn Gate Fix (Elden Ring Nightreign)

**Version 0.1.1-beta.** This is a beta: it works on the cases we could
reproduce, and its log collects details on the cases it may not cover yet
(see [Reporting a problem](#reporting-a-problem)).

Removes the long black screen (about 15 extra seconds) that sometimes follows
a death, most often when you die during the night's tide event.

**[Download the latest version (zip)](https://github.com/Guilhem30/nightreign-respawn-fix/releases/latest/download/respawn-gate-fix.zip)**
· [All releases](https://github.com/Guilhem30/nightreign-respawn-fix/releases)


## Install

**Easy install:** use Mod Engine 3 Manager. Install the mod from its Nexus Mods page:
https://www.nexusmods.com/eldenringnightreign/mods/213

**Manual install** requires [me3](https://github.com/garyttierney/me3).

1. Copy `respawn_gate_fix.dll` into a folder of your Nightreign mods, for
   example `nightreign-mods/gatefix/`.
2. Add it to your me3 profile (`.me3` file):

   ```toml
   [[natives]]
   path = "nightreign-mods/gatefix/respawn_gate_fix.dll"
   ```

3. Start the game through me3.

## Settings

On first run the DLL creates `respawn_gate_fix.ini` next to itself. Changes
apply while the game runs.

| Setting | Default | Effect |
|---|---|---|
| `enabled` | `true` | Turns the whole fix on or off. |
| `verbose` | `true` | Adds details to the log for respawns that start a weather transition or take long. |

## Reporting a problem

`respawn_gate_fix.log`, next to the DLL, gets one line per death with the time
until you were back, and `LONG BLACK SCREEN` when it took 10 s or more. With
`verbose = true`, such respawns also log where you died, the respawn point,
the weather before and after, the in-game clock and what happened to the
scenery fade. If you still get a long black screen with the fix on, please
open an issue and attach that part of the log.

## What causes the long black screen

Each area of the map has a weather. When you respawn in an area whose weather
differs from the place you died, the game starts a 15-second weather
transition: the weather-dependent scenery fades and the lighting blends.

The respawn waits until the world is "stable". Two of the checks behind that
wait for the transition: the scenery counts as not ready while it fades, and
the weather counts as busy until the lighting blend is over. So the whole
transition plays out behind the black screen before you are allowed back in.

## What the fix does

Only while a death respawn is pending:

1. **Skips the weather check.** The game's "is the world stable?" function
   already has an option to skip the weather check (other parts of the game
   use it), but the respawn never does. The DLL changes the respawn's call to
   use it: one 2-byte instruction (`xor edx, edx` → `mov dl, 1`), restored as
   soon as you are back in the world.
2. **Finishes the scenery fade.** Scenery blocks have an "instant" flag that
   makes the game end a fade on the next frame. The DLL sets it on blocks that
   are fading, and clears it again once you are back so that weather changes
   during normal play keep their fade. Both parts are needed: the scenery check waits for the fade too,
   so skipping only the weather check still leaves the black screen.

Everything else still has to be loaded before you respawn: map, collision,
characters and scenery. The lighting blend finishes after the screen fades in,
so the lighting may shift for a few seconds after you respawn.

- The code location is found by a byte pattern, not a fixed address. If an
  update changes that code, the DLL logs "not active" and does nothing.
- Nothing is changed outside a pending respawn, so other scripted events keep
  the game's original behaviour.
- No hooks, and no game files are modified. It runs on a single background
  thread.

## Build

Requires Rust 1.85 or newer (Windows, MSVC toolchain):

```
cargo build --release
```

The DLL is at `target/release/respawn_gate_fix.dll`. The code is pure Rust, so
no C compiler is needed. Every push is built by GitHub Actions
(`.github/workflows/build.yml`). Releases are automatic: when a push to `main`
carries a `Cargo.toml` version that has no release yet, CI creates the tag
`v<version>` and a release with `respawn-gate-fix.zip`, marked as the latest
release. To publish, bump the version and push.

## How it works, in code

`src/lib.rs` is a single commented file: the module comment explains the
cause and the fix, and each constant documents the game offset it uses.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.

This license covers the mod's code only. Elden Ring Nightreign is the property
of its owners; this project contains no game files.

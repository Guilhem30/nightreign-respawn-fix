# Changelog

## 0.2.0-beta

Safety release; the fix works the same way.

- An internal error in the mod now stops the mod, not the game.
- No more writes to scenery that was unloaded in the meantime (for example
  when an expedition ends right after a death).
- Scenery is only changed after it is seen fading twice in a row, and reading
  the scenery list can no longer hang the mod.
- If another mod has changed the same game code, this mod no longer
  overwrites it.

## 0.1.1-beta

First public beta.

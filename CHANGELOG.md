# Changelog

## 0.3.1-beta

Reliability release; the fix works the same way.

- Fixed a rare case where the mod's change to the game could stay in place
  after a respawn. It is now put back, and retried until it is confirmed.
- Scenery changes are now retried after a respawn if the game's scenery
  list can't be read at that moment.
- Optimized release dll size

## 0.3.0-beta

- New on-screen message after a respawn the fix had to handle: "Delayed
  respawn avoided", or a prompt to check the log if it still took long
  (`notify = false` turns it off).
- The message feature only starts when it is first needed, and switches
  itself off if the game behaves unexpectedly; the fix keeps working.
- The mod's code was reorganized into smaller modules, with automated
  tests; the fix behaves the same.

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

//! The geometry blocks of `CSWorldGeomMan` and their weather fade.
//!
//! Each block has a weather fade state (`+0x218`: 0 = idle, 3 = fading) and
//! an "instant" flag (`+0x21e`): while it is set, the game ends a fade on the
//! next frame (`fade_time = fade_length`). The class has no RTTI name.

use crate::mem::Memory;

pub const BLOCK_FADE_STATE: usize = 0x218;
pub const BLOCK_FADE_INSTANT: usize = 0x21e;
const FADE_STATE_FADING: u32 = 3;

/// `CSWorldGeomMan + 0x20`: the head node of the block map.
const MAN_BLOCK_MAP: usize = 0x20;

/// MSVC `std::map` node: `+0x0` left, `+0x8` parent, `+0x10` right, `+0x19`
/// "is nil" flag, `+0x20` the key (block id), `+0x28` the value (block).
const NODE_LEFT: usize = 0x0;
const NODE_PARENT: usize = 0x8;
const NODE_RIGHT: usize = 0x10;
const NODE_IS_NIL: usize = 0x19;
const NODE_KEY: usize = 0x20;
const NODE_VALUE: usize = 0x28;

/// Upper bound on the steps of one walk of the block tree. An in-order walk
/// takes at most about 2 steps per block, so this still allows the 4096
/// blocks the walk used to be capped at.
pub const MAX_TREE_STEPS: u32 = 16384;

/// A block seen fading.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Fading {
    pub id: u32,
    pub block: usize,
    /// The instant flag was already set.
    pub instant: bool,
}

/// All geometry blocks of the manager at `geom_man`, as (block id, block
/// address). The block map is an MSVC `std::map` (red-black tree) whose head
/// node links to the leftmost node; this walks it in order without
/// recursion.
///
/// The game may be editing the tree while we read it. Every step counts
/// against one budget, so a tree that looks inconsistent (even one with a
/// loop) ends the walk instead of keeping our thread busy; the partial list
/// is dropped and the next tick tries again.
///
/// `None` means the map could not be read (no manager, no head, or an
/// inconsistent tree); `Some(empty)` is a map that really has no blocks.
pub fn blocks(mem: &impl Memory, geom_man: usize) -> Option<Vec<(u32, usize)>> {
    let mut blocks = Vec::new();
    if geom_man == 0 {
        return None;
    }
    let head = mem.read_ptr(geom_man + MAN_BLOCK_MAP);
    if head == 0 {
        return None;
    }
    let is_nil = |node: usize| mem.read::<u8>(node + NODE_IS_NIL).unwrap_or(1) != 0;
    let mut steps = 0u32;
    let mut step = || {
        steps += 1;
        steps <= MAX_TREE_STEPS
    };

    let mut node = mem.read_ptr(head + NODE_LEFT); // leftmost node
    while node != 0 && node != head {
        if !step() {
            return None;
        }
        let id = mem.read::<u32>(node + NODE_KEY).unwrap_or(0);
        blocks.push((id, mem.read_ptr(node + NODE_VALUE)));

        // Next node in order: leftmost of the right subtree, or else the
        // first ancestor we reach from its left side.
        let right = mem.read_ptr(node + NODE_RIGHT);
        if right != 0 && !is_nil(right) {
            node = right;
            loop {
                let left = mem.read_ptr(node + NODE_LEFT);
                if left == 0 || is_nil(left) {
                    break;
                }
                if !step() {
                    return None;
                }
                node = left;
            }
        } else {
            let mut parent = mem.read_ptr(node + NODE_PARENT);
            while parent != 0 && !is_nil(parent) && node == mem.read_ptr(parent + NODE_RIGHT) {
                if !step() {
                    return None;
                }
                node = parent;
                parent = mem.read_ptr(node + NODE_PARENT);
            }
            node = parent;
        }
    }
    Some(blocks)
}

/// Blocks that are fading, with their instant flag.
pub fn fading(mem: &impl Memory, geom_man: usize) -> Vec<Fading> {
    blocks(mem, geom_man)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, block)| mem.read::<u32>(block + BLOCK_FADE_STATE) == Some(FADE_STATE_FADING))
        .map(|(id, block)| Fading {
            id,
            block,
            instant: mem
                .read::<u8>(block + BLOCK_FADE_INSTANT)
                .is_some_and(|f| f != 0),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Sparse fake memory: unset bytes are unreadable.
    #[derive(Default)]
    struct Fake(HashMap<usize, u8>);

    impl Fake {
        fn put<T: Copy>(&mut self, addr: usize, value: T) {
            // SAFETY: reads the bytes of a `Copy` value.
            let bytes = unsafe {
                std::slice::from_raw_parts(&value as *const T as *const u8, size_of::<T>())
            };
            for (i, b) in bytes.iter().enumerate() {
                self.0.insert(addr + i, *b);
            }
        }

        /// A tree node (or the head, with `nil`).
        fn node(
            &mut self,
            at: usize,
            left: usize,
            parent: usize,
            right: usize,
            nil: bool,
            id: u32,
        ) {
            self.put(at + NODE_LEFT, left);
            self.put(at + NODE_PARENT, parent);
            self.put(at + NODE_RIGHT, right);
            self.put(at + NODE_IS_NIL, nil as u8);
            self.put(at + NODE_KEY, id);
            self.put(at + NODE_VALUE, BLOCK_BASE + id as usize * 0x1000);
        }
    }

    impl Memory for Fake {
        fn read<T: Copy + Default>(&self, addr: usize) -> Option<T> {
            let bytes: Option<Vec<u8>> = (0..size_of::<T>())
                .map(|i| self.0.get(&(addr + i)).copied())
                .collect();
            // SAFETY: `bytes` holds exactly `size_of::<T>()` bytes; the tests
            // only read integers and pointers, valid for any bit pattern.
            bytes.map(|b| unsafe { std::ptr::read_unaligned(b.as_ptr() as *const T) })
        }
    }

    const MAN: usize = 0x100;
    const HEAD: usize = 0x1000;
    const A: usize = 0x2000;
    const B: usize = 0x3000;
    const C: usize = 0x4000;
    const BLOCK_BASE: usize = 0x10_0000;

    /// B is the root, A its left child, C its right child. MSVC uses the head
    /// as the nil node: head.left = leftmost, head.parent = root, head.right
    /// = rightmost.
    fn three_blocks() -> Fake {
        let mut m = Fake::default();
        m.put(MAN + MAN_BLOCK_MAP, HEAD);
        m.node(HEAD, A, B, C, true, 0);
        m.node(A, HEAD, B, HEAD, false, 1);
        m.node(B, A, HEAD, C, false, 2);
        m.node(C, HEAD, B, HEAD, false, 3);
        m
    }

    fn block(id: u32) -> usize {
        BLOCK_BASE + id as usize * 0x1000
    }

    #[test]
    fn walks_in_order() {
        let m = three_blocks();
        assert_eq!(
            blocks(&m, MAN),
            Some(vec![(1, block(1)), (2, block(2)), (3, block(3))])
        );
    }

    #[test]
    fn empty_or_missing_map() {
        let mut m = Fake::default();
        assert_eq!(blocks(&m, 0), None);
        assert_eq!(blocks(&m, MAN), None, "unreadable manager");
        m.put(MAN + MAN_BLOCK_MAP, HEAD);
        m.node(HEAD, HEAD, HEAD, HEAD, true, 0);
        assert_eq!(
            blocks(&m, MAN),
            Some(Vec::new()),
            "empty map: head links to itself"
        );
    }

    #[test]
    fn loop_in_tree_ends_the_walk() {
        let mut m = three_blocks();
        // C's right child points back to A: an endless in-order walk.
        m.node(C, HEAD, B, A, false, 3);
        assert_eq!(blocks(&m, MAN), None);
    }

    #[test]
    fn finds_fading_blocks() {
        let mut m = three_blocks();
        m.put(block(1) + BLOCK_FADE_STATE, 0u32);
        m.put(block(2) + BLOCK_FADE_STATE, FADE_STATE_FADING);
        m.put(block(2) + BLOCK_FADE_INSTANT, 0u8);
        m.put(block(3) + BLOCK_FADE_STATE, FADE_STATE_FADING);
        m.put(block(3) + BLOCK_FADE_INSTANT, 1u8);
        assert_eq!(
            fading(&m, MAN),
            [
                Fading {
                    id: 2,
                    block: block(2),
                    instant: false
                },
                Fading {
                    id: 3,
                    block: block(3),
                    instant: true
                },
            ]
        );
    }

    #[test]
    fn random_garbage_trees_never_hang_or_panic() {
        use crate::testutil::Rng;
        let addrs: Vec<usize> = (0..12).map(|i| HEAD + i * 0x100).collect();
        for seed in 1..=40u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut m = Fake::default();
            m.put(MAN + MAN_BLOCK_MAP, HEAD);
            let pick = |rng: &mut Rng| match rng.below(8) {
                0 => 0,
                1 => 0xdead_0000_0000,
                _ => addrs[rng.below(addrs.len() as u64) as usize],
            };
            for &a in &addrs {
                let (l, p, r) = (pick(&mut rng), pick(&mut rng), pick(&mut rng));
                m.node(a, l, p, r, rng.below(4) == 0, rng.below(50) as u32);
            }
            for id in 0..50u32 {
                m.put(block(id) + BLOCK_FADE_STATE, rng.below(5) as u32);
            }
            let found = blocks(&m, MAN).unwrap_or_default();
            assert!(found.len() <= MAX_TREE_STEPS as usize, "seed {seed}");
            for f in fading(&m, MAN) {
                assert!(found.contains(&(f.id, f.block)), "seed {seed}");
            }
        }
    }
}

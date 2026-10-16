//! Helpers for the hostile-input tests (test builds only).

use std::ffi::c_void;

use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};

/// Deterministic xorshift64* generator; tests print their seed on failure.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A page of our own, read-write, freed on drop. Reading it after `release`
/// fails, which models memory the game unmapped.
pub struct Page {
    base: usize,
}

impl Page {
    pub fn new() -> Self {
        // SAFETY: a fresh anonymous allocation of one page.
        let p = unsafe { VirtualAlloc(None, 0x1000, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        assert!(!p.is_null(), "VirtualAlloc failed");
        Self { base: p as usize }
    }

    pub fn addr(&self) -> usize {
        self.base
    }

    pub fn put<T: Copy>(&self, offset: usize, value: T) {
        assert!(self.base != 0 && offset + size_of::<T>() <= 0x1000);
        // SAFETY: inside our own committed page.
        unsafe { ((self.base + offset) as *mut T).write_unaligned(value) };
    }

    pub fn get<T: Copy>(&self, offset: usize) -> T {
        assert!(self.base != 0 && offset + size_of::<T>() <= 0x1000);
        // SAFETY: inside our own committed page; callers read integers.
        unsafe { ((self.base + offset) as *const T).read_unaligned() }
    }

    /// Gives the page back to the system; its address stays unreadable.
    pub fn release(&mut self) {
        if self.base != 0 {
            // SAFETY: frees the allocation made in `new`.
            let _ = unsafe { VirtualFree(self.base as *mut c_void, 0, MEM_RELEASE) };
            self.base = 0;
        }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        self.release();
    }
}

/// Value of `EVENT_STATE_RESPAWN_PENDING` in `game.rs`.
const PENDING: u32 = 4;

/// A fake game in real memory of this process: the event state, and a
/// geometry manager with a chain of block nodes (each with its own block page).
/// Mirrors the layouts in `game.rs` and `geom.rs`.
pub struct World {
    _slots: Page,
    _event: Page,
    state: Page,
    man: Page,
    head: Page,
    nodes: Vec<Page>,
    pub blocks: Vec<Page>,
    pub game: crate::game::Game,
}

impl World {
    /// `n` blocks, ids 1..=n, none fading, no respawn pending.
    pub fn new(n: usize) -> Self {
        let slots = Page::new();
        let event = Page::new();
        let state = Page::new();
        let man = Page::new();
        let head = Page::new();
        slots.put(0, event.addr());
        slots.put(8, man.addr());
        event.put(0x10, state.addr());
        man.put(0x20, head.addr());
        head.put(0x19, 1u8); // the head is the nil node
        let nodes: Vec<Page> = (0..n).map(|_| Page::new()).collect();
        let blocks: Vec<Page> = (0..n).map(|_| Page::new()).collect();
        for (i, (node, block)) in nodes.iter().zip(&blocks).enumerate() {
            node.put(0x20, i as u32 + 1);
            node.put(0x28, block.addr());
        }
        let game = crate::game::Game::for_test(slots.addr(), slots.addr() + 8);
        let world = Self {
            _slots: slots,
            _event: event,
            state,
            man,
            head,
            nodes,
            blocks,
            game,
        };
        world.link(&(0..n).collect::<Vec<_>>());
        world
    }

    /// Makes exactly the nodes in `order` the tree, as a chain of right
    /// children (a valid in-order walk).
    pub fn link(&self, order: &[usize]) {
        let head = self.head.addr();
        let first = order.first().map_or(head, |&i| self.nodes[i].addr());
        let last = order.last().map_or(head, |&i| self.nodes[i].addr());
        self.head.put(0, first); // leftmost
        self.head.put(8, first); // root
        self.head.put(0x10, last); // rightmost
        for (k, &i) in order.iter().enumerate() {
            let node = &self.nodes[i];
            node.put(0x19, 0u8);
            node.put(0, head);
            let parent = if k == 0 {
                head
            } else {
                self.nodes[order[k - 1]].addr()
            };
            node.put(8, parent);
            node.put(
                0x10,
                order.get(k + 1).map_or(head, |&j| self.nodes[j].addr()),
            );
        }
    }

    /// Makes the block map (un)readable, as while the game rebuilds it.
    pub fn set_map_readable(&self, readable: bool) {
        self.man
            .put(0x20, if readable { self.head.addr() } else { 0 });
    }

    pub fn set_instant(&self, i: usize, value: u8) {
        self.blocks[i].put(crate::geom::BLOCK_FADE_INSTANT, value);
    }

    pub fn set_pending(&self, pending: bool) {
        self.state.put(8, if pending { PENDING } else { 0 });
    }

    /// Fade state of block `i`: 3 = fading, 0 = idle.
    pub fn set_fading(&self, i: usize, fading: bool) {
        self.blocks[i].put(crate::geom::BLOCK_FADE_STATE, if fading { 3u32 } else { 0 });
    }

    pub fn instant(&self, i: usize) -> u8 {
        self.blocks[i].get(crate::geom::BLOCK_FADE_INSTANT)
    }
}

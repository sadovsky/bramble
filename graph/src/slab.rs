//! Slab arenas with generational slots.
//!
//! Occupancy is encoded in the generation's parity: even means free, odd means
//! live, and zero means never used. That gives two things at once. A single
//! `slot.generation == id.generation` compare validates both identity and
//! liveness, and zeroed memory is a valid empty slab, so the arenas can live in
//! `.bss` with no initialisation pass (DESIGN 3.2, 3.7).

use crate::body::NodeBody;
use crate::edge::Edge;
use crate::id::{NodeId, N_EDGE_KINDS};
use crate::limits::MAX_EDGES;

/// A slot whose generation reaches this is never reused, which removes the
/// wraparound hazard completely at the cost of retiring one slot after two
/// billion allocations of it.
const RETIRE_AT: u32 = u32::MAX - 2;

pub const FLAG_DYING: u8 = 1 << 0;
pub const FLAG_PINNED: u8 = 1 << 1;

/// The per-node bookkeeping shared by every node kind.
///
/// The per-kind list heads are what buy O(1) typed adjacency: "first `Ready`
/// out-edge of this cpu" is one array load, not a scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NodeHeader {
    pub out_head: [u32; N_EDGE_KINDS],
    pub in_head: [u32; N_EDGE_KINDS],
    /// Hot-hop cache of the `Owns` in-edge's source (invariant I9).
    pub owner: NodeId,
    /// Intrusive link for the reaper's pending-deletion list.
    pub next_dying: NodeId,
    pub kind: u8,
    pub flags: u8,
    pub _pad: u16,
}

impl NodeHeader {
    pub const ZERO: NodeHeader = NodeHeader {
        out_head: [0; N_EDGE_KINDS],
        in_head: [0; N_EDGE_KINDS],
        owner: NodeId::NULL,
        next_dying: NodeId::NULL,
        kind: 0,
        flags: 0,
        _pad: 0,
    };

    #[inline]
    pub fn is_dying(&self) -> bool {
        self.flags & FLAG_DYING != 0
    }
    #[inline]
    pub fn head(&self, dir: crate::edge::Dir, kind: crate::id::EdgeKind) -> u32 {
        match dir {
            crate::edge::Dir::Out => self.out_head[kind as usize],
            crate::edge::Dir::In => self.in_head[kind as usize],
        }
    }
    #[inline]
    pub fn set_head(&mut self, dir: crate::edge::Dir, kind: crate::id::EdgeKind, v: u32) {
        match dir {
            crate::edge::Dir::Out => self.out_head[kind as usize] = v,
            crate::edge::Dir::In => self.in_head[kind as usize] = v,
        }
    }
    /// True if the node has no incident edges at all.
    pub fn is_isolated(&self) -> bool {
        self.out_head.iter().all(|&h| h == 0) && self.in_head.iter().all(|&h| h == 0)
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Slot<B> {
    pub generation: u32,
    /// Free-list link, stored as index+1 so that zero means "end of list".
    pub next_free: u32,
    pub hdr: NodeHeader,
    pub body: B,
}

impl<B: NodeBody> Slot<B> {
    pub const ZERO: Slot<B> =
        Slot { generation: 0, next_free: 0, hdr: NodeHeader::ZERO, body: B::ZERO };

    #[inline]
    pub fn is_live(&self) -> bool {
        self.generation & 1 == 1
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Slab<B: NodeBody, const N: usize> {
    pub slots: [Slot<B>; N],
    /// Head of the free list, as index+1.
    free_head: u32,
    /// Next never-used index.
    bump: u32,
    live: u32,
    retired: u32,
}

impl<B: NodeBody, const N: usize> Slab<B, N> {
    pub const ZERO: Slab<B, N> =
        Slab { slots: [Slot::<B>::ZERO; N], free_head: 0, bump: 0, live: 0, retired: 0 };

    #[inline]
    pub fn capacity(&self) -> u32 {
        N as u32
    }
    #[inline]
    pub fn live_count(&self) -> u32 {
        self.live
    }
    #[inline]
    pub fn retired_count(&self) -> u32 {
        self.retired
    }

    /// Allocate a slot and return its index and new generation.
    pub fn alloc(&mut self, body: B) -> Option<(u32, u32)> {
        let idx = if self.free_head != 0 {
            let idx = self.free_head - 1;
            self.free_head = self.slots[idx as usize].next_free;
            idx
        } else if (self.bump as usize) < N {
            let idx = self.bump;
            self.bump += 1;
            idx
        } else {
            return None;
        };

        let slot = &mut self.slots[idx as usize];
        debug_assert!(!slot.is_live());
        slot.generation = slot.generation.wrapping_add(1);
        slot.next_free = 0;
        slot.hdr = NodeHeader::ZERO;
        slot.hdr.kind = B::KIND as u8;
        slot.body = body;
        self.live += 1;
        Some((idx, slot.generation))
    }

    /// Free a slot. The generation is bumped first, so every outstanding id to
    /// the old occupant fails its compare from this instant.
    pub fn free(&mut self, idx: u32) {
        let slot = &mut self.slots[idx as usize];
        debug_assert!(slot.is_live());
        slot.generation = slot.generation.wrapping_add(1);
        slot.hdr = NodeHeader::ZERO;
        slot.body = B::ZERO;
        self.live -= 1;
        if slot.generation >= RETIRE_AT {
            self.retired += 1;
        } else {
            slot.next_free = self.free_head;
            self.free_head = idx + 1;
        }
    }

    #[inline]
    pub fn get(&self, idx: u32, generation: u32) -> Option<&Slot<B>> {
        if generation & 1 == 0 || idx as usize >= N {
            return None;
        }
        let slot = &self.slots[idx as usize];
        if slot.generation == generation {
            Some(slot)
        } else {
            None
        }
    }

    #[inline]
    pub fn get_mut(&mut self, idx: u32, generation: u32) -> Option<&mut Slot<B>> {
        if generation & 1 == 0 || idx as usize >= N {
            return None;
        }
        let slot = &mut self.slots[idx as usize];
        if slot.generation == generation {
            Some(slot)
        } else {
            None
        }
    }

    /// Iterate the indices of every live slot. Used by the checker and the
    /// snapshot writer; never on a fast path.
    pub fn live_indices(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.bump).filter(move |&i| self.slots[i as usize].is_live())
    }
}

/// The edge arena. Slot 0 is a permanent sentinel so that a zeroed list head
/// means "empty list".
#[repr(C)]
pub struct EdgeSlab {
    pub slots: [Edge; MAX_EDGES],
    free_head: u32,
    bump: u32,
    live: u32,
    retired: u32,
}

impl EdgeSlab {
    pub const ZERO: EdgeSlab =
        EdgeSlab { slots: [Edge::ZERO; MAX_EDGES], free_head: 0, bump: 0, live: 0, retired: 0 };

    #[inline]
    pub fn live_count(&self) -> u32 {
        self.live
    }
    #[inline]
    pub fn capacity(&self) -> u32 {
        MAX_EDGES as u32 - 1
    }

    pub fn alloc(&mut self) -> Option<(u32, u32)> {
        if self.bump == 0 {
            self.bump = 1; // reserve the sentinel
        }
        let idx = if self.free_head != 0 {
            let idx = self.free_head - 1;
            self.free_head = self.slots[idx as usize].out_next;
            idx
        } else if (self.bump as usize) < MAX_EDGES {
            let idx = self.bump;
            self.bump += 1;
            idx
        } else {
            return None;
        };
        let e = &mut self.slots[idx as usize];
        debug_assert!(!e.is_live());
        let generation = e.generation.wrapping_add(1);
        *e = Edge::ZERO;
        e.generation = generation;
        self.live += 1;
        Some((idx, generation))
    }

    pub fn free(&mut self, idx: u32) {
        let e = &mut self.slots[idx as usize];
        debug_assert!(e.is_live());
        let generation = e.generation.wrapping_add(1);
        *e = Edge::ZERO;
        e.generation = generation;
        self.live -= 1;
        if generation >= RETIRE_AT {
            self.retired += 1;
        } else {
            e.out_next = self.free_head;
            self.free_head = idx + 1;
        }
    }

    #[inline]
    pub fn get(&self, idx: u32, generation: u32) -> Option<&Edge> {
        if generation & 1 == 0 || idx as usize >= MAX_EDGES {
            return None;
        }
        let e = &self.slots[idx as usize];
        if e.generation == generation {
            Some(e)
        } else {
            None
        }
    }

    #[inline]
    pub fn get_mut(&mut self, idx: u32, generation: u32) -> Option<&mut Edge> {
        if generation & 1 == 0 || idx as usize >= MAX_EDGES {
            return None;
        }
        let e = &mut self.slots[idx as usize];
        if e.generation == generation {
            Some(e)
        } else {
            None
        }
    }

    #[inline]
    pub fn at(&self, idx: u32) -> &Edge {
        &self.slots[idx as usize]
    }
    #[inline]
    pub fn at_mut(&mut self, idx: u32) -> &mut Edge {
        &mut self.slots[idx as usize]
    }

    pub fn live_indices(&self) -> impl Iterator<Item = u32> + '_ {
        (1..self.bump).filter(move |&i| self.slots[i as usize].is_live())
    }
}

// -------------------------------------------------------- list primitives ---
// These take the head pointer *by value* and return the new head. That keeps
// them borrowing only the edge arena, so the caller can copy a head out of a
// node, splice, and write the head back without holding two mutable borrows of
// the graph at once.

use crate::edge::Dir;

/// Insert `e` at the tail of the circular list whose head is `head`.
pub fn list_push_tail(edges: &mut EdgeSlab, head: u32, e: u32, dir: Dir) -> u32 {
    if head == 0 {
        let edge = edges.at_mut(e);
        edge.set_next(dir, e);
        edge.set_prev(dir, e);
        return e;
    }
    let tail = edges.at(head).prev(dir);
    {
        let edge = edges.at_mut(e);
        edge.set_next(dir, head);
        edge.set_prev(dir, tail);
    }
    edges.at_mut(tail).set_next(dir, e);
    edges.at_mut(head).set_prev(dir, e);
    head
}

/// Insert `e` at the head of the circular list. Used for stack-like lists
/// where insertion order does not matter.
pub fn list_push_head(edges: &mut EdgeSlab, head: u32, e: u32, dir: Dir) -> u32 {
    list_push_tail(edges, head, e, dir);
    e
}

/// Remove `e` from the circular list whose head is `head`; returns the new head.
pub fn list_remove(edges: &mut EdgeSlab, head: u32, e: u32, dir: Dir) -> u32 {
    let (next, prev) = {
        let edge = edges.at(e);
        (edge.next(dir), edge.prev(dir))
    };
    if next == e {
        debug_assert_eq!(head, e);
        let edge = edges.at_mut(e);
        edge.set_next(dir, 0);
        edge.set_prev(dir, 0);
        return 0;
    }
    edges.at_mut(prev).set_next(dir, next);
    edges.at_mut(next).set_prev(dir, prev);
    {
        let edge = edges.at_mut(e);
        edge.set_next(dir, 0);
        edge.set_prev(dir, 0);
    }
    if head == e {
        next
    } else {
        head
    }
}

/// Move `e` to the tail of its list. This is `yield`: two splices, no allocation.
pub fn list_move_to_tail(edges: &mut EdgeSlab, head: u32, e: u32, dir: Dir) -> u32 {
    let h = list_remove(edges, head, e, dir);
    list_push_tail(edges, h, e, dir)
}

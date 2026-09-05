//! The invariant checker (DESIGN 4.3).
//!
//! This is not optional tooling. It is the mechanism that keeps the derived
//! indices honest, and it exists before the first index does. It runs in host
//! property tests after every operation, on demand from a syscall, and
//! periodically from the reaper in debug builds. A failure in the kernel is a
//! panic with a graph dump, not a log line.
//!
//! Invariant I5 (page tables are a cache of `Maps` edges) needs the hardware
//! and so lives in the kernel, not here; everything the graph can verify on its
//! own is verified here.

use crate::body::*;
use crate::edge::*;
use crate::graph::*;
use crate::id::*;
use crate::limits::*;
use crate::slab::NodeHeader;

const BITMAP_WORDS: usize = MAX_EDGES / 64;

/// The checker deliberately does not use the graph's own `Result` alias.
pub type CheckResult = core::result::Result<(), Violation>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Violation {
    /// I1: an edge names an endpoint that is not a live node.
    DanglingEndpoint { edge: EdgeId, endpoint: NodeId },
    /// I1: an edge is not threaded into both of its endpoints' lists.
    EdgeMissingFromOutList { edge: EdgeId },
    EdgeMissingFromInList { edge: EdgeId },
    /// I1: an adjacency list contains a dead or duplicated edge.
    DeadEdgeInList { node: NodeId, edge_idx: u32 },
    DuplicateEdgeInList { node: NodeId, edge_idx: u32 },
    /// I1: `next`/`prev` disagree.
    BrokenBackLink { node: NodeId, edge_idx: u32 },
    /// I1: an edge in a node's list does not name that node as its endpoint.
    ListEndpointMismatch { node: NodeId, edge_idx: u32 },
    /// I1: an edge in a kind's list is of another kind.
    ListKindMismatch { node: NodeId, edge_idx: u32, expected: EdgeKind },
    /// A node's header disagrees with the kind encoded in its id.
    HeaderKindMismatch { node: NodeId, header_kind: u8 },
    /// I3: this edge kind does not accept these endpoint kinds.
    Incompatible { edge: EdgeId, kind: EdgeKind, src: NodeKind, dst: NodeKind },
    /// I2: every live non-root node has exactly one owner.
    NoOwner { node: NodeId },
    MultipleOwners { node: NodeId },
    /// I2: following owners must reach the root.
    OwnershipCycle { node: NodeId },
    /// I4: a dying node must be detached from the tree and the scheduler.
    DyingStillOwned { node: NodeId },
    DyingStillScheduled { node: NodeId },
    /// I6: thread state and scheduler edges disagree.
    ThreadStateMismatch { thread: NodeId, state: ThreadState, ready: bool, waiting: bool },
    /// I8: the handle table and the `Holds` edges disagree.
    HandleSlotDead { process: NodeId, slot: u32 },
    HandleSlotMismatch { process: NodeId, slot: u32 },
    HoldsEdgeNotInTable { edge: EdgeId },
    /// I9: a hot-hop cache diverged from the edge it shadows.
    OwnerCacheStale { node: NodeId, cached: NodeId, actual: NodeId },
    Cr3CacheStale { thread: NodeId, cached: u64, actual: u64 },
    MapCountStale { node: NodeId, cached: u32, actual: u32 },
    MappingCountStale { space: NodeId, cached: u32, actual: u32 },
    /// I7: two mappings in one address space cover the same virtual page.
    MappingOverlap { space: NodeId, a: EdgeId, b: EdgeId },
    /// I11: the range index disagrees with the `Maps` edges it indexes.
    RangeIndexUnsorted { space: NodeId, at: usize },
    RangeIndexStale { space: NodeId, at: usize },
    RangeIndexCount { space: NodeId, indexed: u32, edges: u32 },
    RangeIndexMissing { space: NodeId, edge: EdgeId },
    /// The walk exceeded its budget, which means a list is corrupt.
    WalkOverrun { node: NodeId },
    /// A single-valued relationship has more than one edge. This is the
    /// invariant whose absence first desynchronised the `cr3` cache.
    Cardinality { node: NodeId, kind: EdgeKind, dir: &'static str, count: usize },
}

/// Scratch space for a check. Held by the caller so the kernel can put it in
/// `.bss` rather than on a 16 KiB kernel stack.
pub struct Checker {
    seen_out: [u64; BITMAP_WORDS],
    seen_in: [u64; BITMAP_WORDS],
}

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

impl Checker {
    pub const fn new() -> Checker {
        Checker { seen_out: [0; BITMAP_WORDS], seen_in: [0; BITMAP_WORDS] }
    }

    fn clear(&mut self) {
        self.seen_out = [0; BITMAP_WORDS];
        self.seen_in = [0; BITMAP_WORDS];
    }

    fn mark(bits: &mut [u64; BITMAP_WORDS], i: u32) -> bool {
        let (w, b) = ((i / 64) as usize, i % 64);
        let was = bits[w] & (1 << b) != 0;
        bits[w] |= 1 << b;
        !was
    }

    fn is_marked(bits: &[u64; BITMAP_WORDS], i: u32) -> bool {
        bits[(i / 64) as usize] & (1 << (i % 64)) != 0
    }

    /// Verify every invariant the graph can check without hardware.
    pub fn check(&mut self, g: &Graph) -> CheckResult {
        self.clear();
        self.check_lists(g)?;
        self.check_edges(g)?;
        self.check_ownership(g)?;
        self.check_scheduler(g)?;
        self.check_handles(g)?;
        self.check_cardinality(g)?;
        self.check_range_index(g)?;
        self.check_caches(g)?;
        Ok(())
    }

    /// Walk every adjacency list once, marking edges seen in each direction.
    /// O(E) in total, not O(E * degree).
    fn check_lists(&mut self, g: &Graph) -> CheckResult {
        for node in g.live_nodes() {
            let hdr: &NodeHeader = g.header(node).expect("live node has a header");
            if hdr.kind != node.kind_raw() {
                return Err(Violation::HeaderKindMismatch { node, header_kind: hdr.kind });
            }
            for k in 0..N_EDGE_KINDS {
                let kind = EdgeKind::from_u8(k as u8).expect("kind in range");
                for dir in [Dir::Out, Dir::In] {
                    let head = hdr.head(dir, kind);
                    if head == 0 {
                        continue;
                    }
                    let mut cur = head;
                    let mut budget = MAX_EDGES as u32 + 1;
                    loop {
                        if budget == 0 {
                            return Err(Violation::WalkOverrun { node });
                        }
                        budget -= 1;
                        let e = g.edges.at(cur);
                        if !e.is_live() {
                            return Err(Violation::DeadEdgeInList { node, edge_idx: cur });
                        }
                        if e.kind != kind as u8 {
                            return Err(Violation::ListKindMismatch {
                                node,
                                edge_idx: cur,
                                expected: kind,
                            });
                        }
                        let endpoint = match dir {
                            Dir::Out => e.src,
                            Dir::In => e.dst,
                        };
                        if endpoint != node {
                            return Err(Violation::ListEndpointMismatch { node, edge_idx: cur });
                        }
                        let next = e.next(dir);
                        if g.edges.at(next).prev(dir) != cur {
                            return Err(Violation::BrokenBackLink { node, edge_idx: cur });
                        }
                        let bits = match dir {
                            Dir::Out => &mut self.seen_out,
                            Dir::In => &mut self.seen_in,
                        };
                        if !Self::mark(bits, cur) {
                            return Err(Violation::DuplicateEdgeInList { node, edge_idx: cur });
                        }
                        if next == head {
                            break;
                        }
                        cur = next;
                    }
                }
            }
        }
        Ok(())
    }

    /// I1 and I3: every live edge is threaded into both lists and is well typed.
    fn check_edges(&mut self, g: &Graph) -> CheckResult {
        for eid in g.live_edges() {
            let e = g.edge(eid).expect("live edge");
            let i = eid.idx();
            if !Self::is_marked(&self.seen_out, i) {
                return Err(Violation::EdgeMissingFromOutList { edge: eid });
            }
            if !Self::is_marked(&self.seen_in, i) {
                return Err(Violation::EdgeMissingFromInList { edge: eid });
            }
            if g.header(e.src).is_none() {
                return Err(Violation::DanglingEndpoint { edge: eid, endpoint: e.src });
            }
            if g.header(e.dst).is_none() {
                return Err(Violation::DanglingEndpoint { edge: eid, endpoint: e.dst });
            }
            let (kind, sk, dk) = (
                e.edge_kind().expect("valid kind"),
                e.src.kind().expect("valid src kind"),
                e.dst.kind().expect("valid dst kind"),
            );
            if !compatible(kind, sk, dk) {
                return Err(Violation::Incompatible { edge: eid, kind, src: sk, dst: dk });
            }
        }
        Ok(())
    }

    /// I2 and I4: `Owns` is a tree rooted at `Root`, and dying nodes are out of it.
    fn check_ownership(&self, g: &Graph) -> CheckResult {
        let root = g.root().map(|r| r.id());
        for node in g.live_nodes() {
            let hdr = g.header(node).expect("live");
            let mut owners = g.in_edges(node, EdgeKind::Owns);
            let first = owners.next();
            let extra = owners.next();
            if extra.is_some() {
                return Err(Violation::MultipleOwners { node });
            }

            if hdr.is_dying() {
                if first.is_some() {
                    return Err(Violation::DyingStillOwned { node });
                }
                continue;
            }
            if Some(node) == root {
                if first.is_some() {
                    return Err(Violation::MultipleOwners { node });
                }
                continue;
            }
            let e = match first {
                Some(e) => e,
                None => return Err(Violation::NoOwner { node }),
            };
            let actual = g.edge(e).expect("live edge").src;
            if hdr.owner != actual {
                return Err(Violation::OwnerCacheStale { node, cached: hdr.owner, actual });
            }

            // Follow the chain to the root; the tree has no cycles.
            let mut cur = actual;
            let mut budget = MAX_PROCESSES + MAX_THREADS + MAX_MEMOBJS + MAX_SPACES + 8;
            loop {
                if Some(cur) == root {
                    break;
                }
                if budget == 0 || cur.is_null() {
                    return Err(Violation::OwnershipCycle { node });
                }
                budget -= 1;
                match g.header(cur) {
                    Some(h) if !h.owner.is_null() => cur = h.owner,
                    // A dying ancestor has already been detached; its subtree is
                    // in the reaper's hands and is not part of the live tree.
                    Some(h) if h.is_dying() => break,
                    _ => return Err(Violation::OwnershipCycle { node }),
                }
            }
        }
        Ok(())
    }

    /// I4 and I6: scheduler edges agree with thread state.
    fn check_scheduler(&self, g: &Graph) -> CheckResult {
        for node in g.live_nodes() {
            if node.kind() != Some(NodeKind::Thread) {
                continue;
            }
            let t: Ref<Thread> = g.typed(node).expect("thread");
            let hdr = g.header(node).expect("live");
            let ready = g.first_in(node, EdgeKind::Ready).is_some();
            let waiting = g.first_out(node, EdgeKind::Waiting).is_some();

            if hdr.is_dying() && (ready || waiting) {
                return Err(Violation::DyingStillScheduled { node });
            }
            let state = g.body(t).expect("body").state;
            let ok = match state {
                ThreadState::Ready => ready && !waiting,
                ThreadState::Blocked => waiting && !ready,
                ThreadState::Running | ThreadState::Inert => !ready && !waiting,
                ThreadState::Dying => !ready && !waiting,
            };
            if !ok {
                return Err(Violation::ThreadStateMismatch { thread: node, state, ready, waiting });
            }
        }
        Ok(())
    }

    /// I8: the handle table and the `Holds` edges are two views of one thing.
    fn check_handles(&mut self, g: &Graph) -> CheckResult {
        for node in g.live_nodes() {
            if node.kind() != Some(NodeKind::Process) {
                continue;
            }
            let p: Ref<Process> = g.typed(node).expect("process");
            let body = g.body(p).expect("body");
            for (slot, &ei) in body.handles.iter().enumerate().skip(1) {
                if ei == 0 {
                    continue;
                }
                let e = g.edges.at(ei);
                if !e.is_live() || e.kind != EdgeKind::Holds as u8 {
                    return Err(Violation::HandleSlotDead { process: node, slot: slot as u32 });
                }
                if e.src != node || HoldsAttr::decode(e.data).slot != slot as u32 {
                    return Err(Violation::HandleSlotMismatch { process: node, slot: slot as u32 });
                }
            }
            // And the converse: no orphan capability edges.
            for eid in g.out_edges(node, EdgeKind::Holds) {
                let e = g.edge(eid).expect("live");
                let slot = HoldsAttr::decode(e.data).slot as usize;
                if slot == 0 || slot >= HANDLE_SLOTS || body.handles[slot] != eid.idx() {
                    return Err(Violation::HoldsEdgeNotInTable { edge: eid });
                }
            }
        }
        Ok(())
    }

    /// Single-valued relationships really are single-valued (DESIGN 4.2).
    fn check_cardinality(&self, g: &Graph) -> CheckResult {
        for node in g.live_nodes() {
            if node.kind() != Some(NodeKind::Thread) {
                continue;
            }
            for (kind, dir) in [
                (EdgeKind::InSpace, Dir::Out),
                (EdgeKind::Waiting, Dir::Out),
                (EdgeKind::Ready, Dir::In),
            ] {
                let count = match dir {
                    Dir::Out => g.out_edges(node, kind).count(),
                    Dir::In => g.in_edges(node, kind).count(),
                };
                if count > 1 {
                    let d = if dir == Dir::Out { "out" } else { "in" };
                    return Err(Violation::Cardinality { node, kind, dir: d, count });
                }
            }
        }
        Ok(())
    }

    /// I11: the range index is exactly the `Maps` edges, in address order.
    ///
    /// This is the newest derived index and so the most likely to drift. It is
    /// also the one whose drift would be least visible: a stale entry does not
    /// break anything until a page fault lands on it, at which point a process
    /// is handed the wrong memory.
    fn check_range_index(&self, g: &Graph) -> CheckResult {
        for node in g.live_nodes() {
            if node.kind() != Some(NodeKind::AddressSpace) {
                continue;
            }
            let space: Ref<AddressSpace> = g.typed(node).expect("address space");
            let body = g.body(space).expect("body");
            let count = body.mapping_count as usize;
            if count > MAX_MAPPINGS_PER_SPACE {
                return Err(Violation::RangeIndexCount {
                    space: node,
                    indexed: body.mapping_count,
                    edges: 0,
                });
            }

            let edges = g.out_edges(node, EdgeKind::Maps).count();
            if edges != count {
                return Err(Violation::RangeIndexCount {
                    space: node,
                    indexed: body.mapping_count,
                    edges: edges as u32,
                });
            }

            let mut previous_end = 0u64;
            for (i, &ei) in body.ranges[..count].iter().enumerate() {
                let edge = match g.edge(EdgeId::new(ei, g.edge_generation(ei))) {
                    Some(e) if e.kind == EdgeKind::Maps as u8 && e.src == node => e,
                    _ => return Err(Violation::RangeIndexStale { space: node, at: i }),
                };
                let attr = MapsAttr::decode(edge.data);
                // Sorted, and by I7 also disjoint, so each range must start at
                // or after the previous one ended.
                if attr.vaddr < previous_end {
                    return Err(Violation::RangeIndexUnsorted { space: node, at: i });
                }
                previous_end = attr.end();
            }

            // And nothing indexed that is not an edge, or the reverse.
            for eid in g.out_edges(node, EdgeKind::Maps) {
                if !body.ranges[..count].contains(&eid.idx()) {
                    return Err(Violation::RangeIndexMissing { space: node, edge: eid });
                }
            }
        }
        Ok(())
    }

    /// I7 and I9: mappings do not overlap, and every cache equals its edge.
    fn check_caches(&self, g: &Graph) -> CheckResult {
        for node in g.live_nodes() {
            match node.kind() {
                Some(NodeKind::Thread) => {
                    let t: Ref<Thread> = g.typed(node).expect("thread");
                    let cached = g.body(t).expect("body").cr3;
                    let actual = match g.space_of(t) {
                        Some(s) => g.body(s).map(|b| b.pml4_phys).unwrap_or(0),
                        None => 0,
                    };
                    if cached != actual {
                        return Err(Violation::Cr3CacheStale { thread: node, cached, actual });
                    }
                }
                Some(NodeKind::MemoryObject) => {
                    let m: Ref<MemoryObject> = g.typed(node).expect("memobj");
                    let cached = g.body(m).expect("body").map_count;
                    let actual = g.in_edges(node, EdgeKind::Maps).count() as u32;
                    if cached != actual {
                        return Err(Violation::MapCountStale { node, cached, actual });
                    }
                }
                Some(NodeKind::AddressSpace) => {
                    let s: Ref<AddressSpace> = g.typed(node).expect("space");
                    let cached = g.body(s).expect("body").mapping_count;
                    let mut count = 0u32;
                    let mut ids = [EdgeId::NULL; MAX_WALK];
                    for (n, e) in g.out_edges(node, EdgeKind::Maps).enumerate() {
                        if n < MAX_WALK {
                            ids[n] = e;
                            count += 1;
                        }
                    }
                    if cached != count {
                        return Err(Violation::MappingCountStale {
                            space: node,
                            cached,
                            actual: count,
                        });
                    }
                    for i in 0..count as usize {
                        let a = MapsAttr::decode(g.edge(ids[i]).expect("live").data);
                        for j in i + 1..count as usize {
                            let b = MapsAttr::decode(g.edge(ids[j]).expect("live").data);
                            if a.overlaps(&b) {
                                return Err(Violation::MappingOverlap {
                                    space: node,
                                    a: ids[i],
                                    b: ids[j],
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

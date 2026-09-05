//! The graph itself: eight typed node arenas, one edge arena, and the
//! operations that maintain the invariants of DESIGN 4.3.
//!
//! Every mutating operation here is O(1) or O(small), except the two that are
//! explicitly deferred to thread context: cascading deletion (via `reap_step`,
//! which is itself O(1) per call) and snapshotting.

use core::marker::PhantomData;

use crate::body::*;
use crate::edge::*;
use crate::id::*;
use crate::limits::*;
use crate::slab::*;

// ------------------------------------------------------------------ error ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphError {
    /// A node arena of this kind is full.
    ArenaFull(NodeKind),
    EdgeArenaFull,
    /// The id refers to a slot that has been freed and possibly reused.
    StaleNode(NodeId),
    StaleEdge(EdgeId),
    /// The edge kind does not accept this pair of endpoint kinds (I3).
    Incompatible { kind: EdgeKind, src: NodeKind, dst: NodeKind },
    /// The node is being destroyed and may not gain new relationships (I4).
    Dying(NodeId),
    /// The process's handle table is full.
    NoFreeSlot,
    EmptySlot(u32),
    /// Capability lacks a right the operation requires.
    MissingRights { have: Rights, need: Rights },
    NameTooLong,
    /// A root already exists; there is exactly one.
    RootExists,
    /// The virtual range collides with an existing mapping (I7).
    RangeOverlap,
    /// The thread is not in a state that permits this transition (I6).
    BadState,
}

pub type Result<T> = core::result::Result<T, GraphError>;

// -------------------------------------------------------------------- ref ---

/// A node id that remembers its body type, so that the `link_*` methods can
/// enforce endpoint compatibility (invariant I3) at compile time.
#[repr(transparent)]
pub struct Ref<B: NodeBody> {
    id: NodeId,
    _p: PhantomData<fn() -> B>,
}

impl<B: NodeBody> Ref<B> {
    #[inline]
    pub const fn id(self) -> NodeId {
        self.id
    }
    #[inline]
    pub(crate) const fn from_raw(id: NodeId) -> Ref<B> {
        Ref { id, _p: PhantomData }
    }
}

impl<B: NodeBody> Clone for Ref<B> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<B: NodeBody> Copy for Ref<B> {}
impl<B: NodeBody> PartialEq for Ref<B> {
    fn eq(&self, o: &Self) -> bool {
        self.id == o.id
    }
}
impl<B: NodeBody> Eq for Ref<B> {}
impl<B: NodeBody> core::fmt::Debug for Ref<B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.id)
    }
}

/// Per-body-type access to the right arena. Implemented once per node kind by
/// the macro below; it is what lets `create_*` and `body` be generic without
/// any dynamic dispatch or unsafe casting.
pub trait Store<B: NodeBody> {
    fn slot(&self, idx: u32, generation: u32) -> Option<&Slot<B>>;
    fn slot_mut(&mut self, idx: u32, generation: u32) -> Option<&mut Slot<B>>;
    fn arena_alloc(&mut self, body: B) -> Option<(u32, u32)>;
}

// ------------------------------------------------------------------ graph ---

#[repr(C)]
pub struct Graph {
    pub roots: Slab<Root, MAX_ROOTS>,
    pub cpus: Slab<Cpu, MAX_CPUS>,
    pub processes: Slab<Process, MAX_PROCESSES>,
    pub threads: Slab<Thread, MAX_THREADS>,
    pub spaces: Slab<AddressSpace, MAX_SPACES>,
    pub memobjs: Slab<MemoryObject, MAX_MEMOBJS>,
    pub endpoints: Slab<Endpoint, MAX_ENDPOINTS>,
    pub devices: Slab<Device, MAX_DEVICES>,
    pub edges: EdgeSlab,
    /// Bumped on every structural mutation; a snapshot carries it so a reader
    /// can tell that two snapshots straddle a change (DESIGN 5.4).
    seq: u64,
    /// Head of the reaper's pending-deletion list.
    dying_head: NodeId,
    root_id: NodeId,
}

macro_rules! impl_store {
    ($body:ty, $field:ident, $kind:expr) => {
        impl Store<$body> for Graph {
            #[inline]
            fn slot(&self, idx: u32, generation: u32) -> Option<&Slot<$body>> {
                self.$field.get(idx, generation)
            }
            #[inline]
            fn slot_mut(&mut self, idx: u32, generation: u32) -> Option<&mut Slot<$body>> {
                self.$field.get_mut(idx, generation)
            }
            #[inline]
            fn arena_alloc(&mut self, body: $body) -> Option<(u32, u32)> {
                self.$field.alloc(body)
            }
        }
    };
}

impl_store!(Root, roots, NodeKind::Root);
impl_store!(Cpu, cpus, NodeKind::Cpu);
impl_store!(Process, processes, NodeKind::Process);
impl_store!(Thread, threads, NodeKind::Thread);
impl_store!(AddressSpace, spaces, NodeKind::AddressSpace);
impl_store!(MemoryObject, memobjs, NodeKind::MemoryObject);
impl_store!(Endpoint, endpoints, NodeKind::Endpoint);
impl_store!(Device, devices, NodeKind::Device);

/// Physical resources a freed node was holding, which only the kernel knows
/// how to return. The graph reports them; it never touches them itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reclaim {
    Nothing,
    /// Frames a memory object described. Device and pinned regions are
    /// reported too, with their flags, so the caller can decline to free them.
    Frames { phys: u64, pages: u32, flags: MemFlags },
    /// The page tables of an address space, root included.
    PageTables { pml4_phys: u64 },
}

/// What one call to `reap_step` accomplished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReapStep {
    /// Nothing is pending.
    Idle,
    /// Bounded work was done; call again.
    Progress,
    /// A node's slot was released, along with whatever physical resources it
    /// was holding.
    Freed { id: NodeId, reclaim: Reclaim },
    /// A thread's wait was torn down because its target is being destroyed.
    /// The kernel should resume it with an error.
    AbortedWait { thread: NodeId },
}

impl Graph {
    pub const EMPTY: Graph = Graph {
        roots: Slab::ZERO,
        cpus: Slab::ZERO,
        processes: Slab::ZERO,
        threads: Slab::ZERO,
        spaces: Slab::ZERO,
        memobjs: Slab::ZERO,
        endpoints: Slab::ZERO,
        devices: Slab::ZERO,
        edges: EdgeSlab::ZERO,
        seq: 0,
        dying_head: NodeId::NULL,
        root_id: NodeId::NULL,
    };

    #[inline]
    pub fn seq(&self) -> u64 {
        self.seq
    }
    #[inline]
    pub fn root(&self) -> Option<Ref<Root>> {
        if self.root_id.is_null() {
            None
        } else {
            Some(Ref::from_raw(self.root_id))
        }
    }
    #[inline]
    pub fn node_count(&self) -> u32 {
        self.roots.live_count()
            + self.cpus.live_count()
            + self.processes.live_count()
            + self.threads.live_count()
            + self.spaces.live_count()
            + self.memobjs.live_count()
            + self.endpoints.live_count()
            + self.devices.live_count()
    }
    #[inline]
    pub fn edge_count(&self) -> u32 {
        self.edges.live_count()
    }
    /// Fail early if the edge arena cannot take one more edge, so that
    /// multi-step operations never half-apply.
    #[inline]
    fn reserve_edge(&self) -> Result<()> {
        if self.edges.live_count() >= self.edges.capacity() {
            Err(GraphError::EdgeArenaFull)
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn has_pending_reap(&self) -> bool {
        !self.dying_head.is_null()
    }

    // ------------------------------------------------------------ headers ---

    /// Type-erased header lookup. The kind lives in the id, so this is a jump
    /// table over eight arenas, not a search.
    #[inline]
    pub fn header(&self, id: NodeId) -> Option<&NodeHeader> {
        let (i, g) = (id.idx(), id.generation());
        Some(match id.kind()? {
            NodeKind::Root => &self.roots.get(i, g)?.hdr,
            NodeKind::Cpu => &self.cpus.get(i, g)?.hdr,
            NodeKind::Process => &self.processes.get(i, g)?.hdr,
            NodeKind::Thread => &self.threads.get(i, g)?.hdr,
            NodeKind::AddressSpace => &self.spaces.get(i, g)?.hdr,
            NodeKind::MemoryObject => &self.memobjs.get(i, g)?.hdr,
            NodeKind::Endpoint => &self.endpoints.get(i, g)?.hdr,
            NodeKind::Device => &self.devices.get(i, g)?.hdr,
        })
    }

    #[inline]
    pub fn header_mut(&mut self, id: NodeId) -> Option<&mut NodeHeader> {
        let (i, g) = (id.idx(), id.generation());
        Some(match id.kind()? {
            NodeKind::Root => &mut self.roots.get_mut(i, g)?.hdr,
            NodeKind::Cpu => &mut self.cpus.get_mut(i, g)?.hdr,
            NodeKind::Process => &mut self.processes.get_mut(i, g)?.hdr,
            NodeKind::Thread => &mut self.threads.get_mut(i, g)?.hdr,
            NodeKind::AddressSpace => &mut self.spaces.get_mut(i, g)?.hdr,
            NodeKind::MemoryObject => &mut self.memobjs.get_mut(i, g)?.hdr,
            NodeKind::Endpoint => &mut self.endpoints.get_mut(i, g)?.hdr,
            NodeKind::Device => &mut self.devices.get_mut(i, g)?.hdr,
        })
    }

    #[inline]
    pub fn is_live(&self, id: NodeId) -> bool {
        self.header(id).is_some()
    }

    #[inline]
    pub fn is_dying(&self, id: NodeId) -> bool {
        self.header(id).is_some_and(|h| h.is_dying())
    }

    fn free_slot(&mut self, id: NodeId) {
        let i = id.idx();
        match id.kind() {
            Some(NodeKind::Root) => self.roots.free(i),
            Some(NodeKind::Cpu) => self.cpus.free(i),
            Some(NodeKind::Process) => self.processes.free(i),
            Some(NodeKind::Thread) => self.threads.free(i),
            Some(NodeKind::AddressSpace) => self.spaces.free(i),
            Some(NodeKind::MemoryObject) => self.memobjs.free(i),
            Some(NodeKind::Endpoint) => self.endpoints.free(i),
            Some(NodeKind::Device) => self.devices.free(i),
            None => {}
        }
    }

    // -------------------------------------------------------- node bodies ---

    #[inline]
    pub fn body<B: NodeBody>(&self, r: Ref<B>) -> Option<&B>
    where
        Self: Store<B>,
    {
        self.slot(r.id.idx(), r.id.generation()).map(|s| &s.body)
    }

    #[inline]
    pub fn body_mut<B: NodeBody>(&mut self, r: Ref<B>) -> Option<&mut B>
    where
        Self: Store<B>,
    {
        let (i, g) = (r.id.idx(), r.id.generation());
        self.slot_mut(i, g).map(|s| &mut s.body)
    }

    /// Recover a typed reference from a raw id, checking the kind at runtime.
    /// This is the boundary where syscall arguments become typed.
    pub fn typed<B: NodeBody>(&self, id: NodeId) -> Option<Ref<B>>
    where
        Self: Store<B>,
    {
        if id.kind_raw() != B::KIND as u8 {
            return None;
        }
        self.slot(id.idx(), id.generation())?;
        Some(Ref::from_raw(id))
    }

    // ------------------------------------------------------ node creation ---

    /// Create the one and only root.
    pub fn create_root(&mut self) -> Result<Ref<Root>> {
        if !self.root_id.is_null() {
            return Err(GraphError::RootExists);
        }
        let (i, g) = self.roots.alloc(Root::ZERO).ok_or(GraphError::ArenaFull(NodeKind::Root))?;
        let id = NodeId::new(NodeKind::Root, i, g);
        self.root_id = id;
        self.seq += 1;
        Ok(Ref::from_raw(id))
    }

    /// Create a node owned by the root. The `Owns` edge is created with the
    /// node, which is how invariant I2 holds by construction.
    pub fn create_under_root<B: RootOwnable>(&mut self, owner: Ref<Root>, body: B) -> Result<Ref<B>>
    where
        Self: Store<B>,
    {
        self.create_owned(owner.id, body)
    }

    /// Create a node owned by a process.
    pub fn create_under_process<B: ProcessOwnable>(
        &mut self,
        owner: Ref<Process>,
        body: B,
    ) -> Result<Ref<B>>
    where
        Self: Store<B>,
    {
        self.create_owned(owner.id, body)
    }

    fn create_owned<B: NodeBody>(&mut self, owner: NodeId, body: B) -> Result<Ref<B>>
    where
        Self: Store<B>,
    {
        let oh = self.header(owner).ok_or(GraphError::StaleNode(owner))?;
        if oh.is_dying() {
            return Err(GraphError::Dying(owner));
        }
        // Reserve first: an arena-full failure must not leave a parentless node.
        self.reserve_edge()?;
        let (i, g) = self.arena_alloc(body).ok_or(GraphError::ArenaFull(B::KIND))?;
        let id = NodeId::new(B::KIND, i, g);
        match self.link_raw(owner, EdgeKind::Owns, id, RawEdgeData::ZERO) {
            Ok(_) => {}
            Err(e) => {
                self.free_slot(id);
                return Err(e);
            }
        }
        Ok(Ref::from_raw(id))
    }

    // -------------------------------------------------------------- edges ---

    /// Everything `link_raw` would reject, checked without mutating anything.
    ///
    /// Multi-step operations call this first so that a rejected transition
    /// leaves the graph exactly as it found it. Skipping it is what let a
    /// thread end up Ready with no run-queue edge: the ready edge had already
    /// been unlinked when the link to a dying endpoint failed.
    pub fn precheck_link(&self, src: NodeId, kind: EdgeKind, dst: NodeId) -> Result<()> {
        let sk = src.kind().ok_or(GraphError::StaleNode(src))?;
        let dk = dst.kind().ok_or(GraphError::StaleNode(dst))?;
        if !compatible(kind, sk, dk) {
            return Err(GraphError::Incompatible { kind, src: sk, dst: dk });
        }
        let sh = self.header(src).ok_or(GraphError::StaleNode(src))?;
        if sh.is_dying() {
            return Err(GraphError::Dying(src));
        }
        let dh = self.header(dst).ok_or(GraphError::StaleNode(dst))?;
        if dh.is_dying() {
            return Err(GraphError::Dying(dst));
        }
        self.reserve_edge()
    }

    /// The generic edge constructor. Checks compatibility at runtime; the
    /// typed `link_*` wrappers below make that check redundant at their call
    /// sites, which is the point of invariant I3's compile-time half.
    pub fn link_raw(
        &mut self,
        src: NodeId,
        kind: EdgeKind,
        dst: NodeId,
        data: RawEdgeData,
    ) -> Result<EdgeId> {
        let sk = src.kind().ok_or(GraphError::StaleNode(src))?;
        let dk = dst.kind().ok_or(GraphError::StaleNode(dst))?;
        if !compatible(kind, sk, dk) {
            return Err(GraphError::Incompatible { kind, src: sk, dst: dk });
        }
        {
            let sh = self.header(src).ok_or(GraphError::StaleNode(src))?;
            if sh.is_dying() {
                return Err(GraphError::Dying(src));
            }
            let dh = self.header(dst).ok_or(GraphError::StaleNode(dst))?;
            if dh.is_dying() {
                return Err(GraphError::Dying(dst));
            }
        }

        let (ei, eg) = self.edges.alloc().ok_or(GraphError::EdgeArenaFull)?;
        {
            let e = self.edges.at_mut(ei);
            e.src = src;
            e.dst = dst;
            e.kind = kind as u8;
            e.data = data;
        }

        // Copy the head out, splice, write it back. This borrows only the edge
        // arena at a time, which is what keeps SMP's per-node locking possible
        // later (DESIGN 3.8 rule 1).
        let out_head = self.header(src).unwrap().head(Dir::Out, kind);
        let new_out = list_push_tail(&mut self.edges, out_head, ei, Dir::Out);
        self.header_mut(src).unwrap().set_head(Dir::Out, kind, new_out);

        let in_head = self.header(dst).unwrap().head(Dir::In, kind);
        let new_in = list_push_tail(&mut self.edges, in_head, ei, Dir::In);
        self.header_mut(dst).unwrap().set_head(Dir::In, kind, new_in);

        if kind == EdgeKind::Owns {
            self.header_mut(dst).unwrap().owner = src;
        }

        self.seq += 1;
        Ok(EdgeId::new(ei, eg))
    }

    /// Remove an edge, performing the bookkeeping its kind implies. Keeping
    /// that bookkeeping here, in the single place edges disappear, is what
    /// keeps the derived indices (I8, I9) honest during teardown.
    pub fn unlink(&mut self, id: EdgeId) -> Result<()> {
        let (ei, eg) = (id.idx(), id.generation());
        let e = *self.edges.get(ei, eg).ok_or(GraphError::StaleEdge(id))?;
        let kind = e.edge_kind().ok_or(GraphError::StaleEdge(id))?;

        match kind {
            EdgeKind::Holds => {
                // Keep the handle table consistent (I8).
                let slot = HoldsAttr::decode(e.data).slot;
                if let Some(p) = self.typed::<Process>(e.src) {
                    if let Some(b) = self.body_mut(p) {
                        if (slot as usize) < HANDLE_SLOTS && b.handles[slot as usize] == ei {
                            b.handles[slot as usize] = 0;
                        }
                    }
                }
            }
            EdgeKind::Maps => {
                if let Some(m) = self.typed::<MemoryObject>(e.dst) {
                    if let Some(b) = self.body_mut(m) {
                        b.map_count = b.map_count.saturating_sub(1);
                    }
                }
                if let Some(s) = self.typed::<AddressSpace>(e.src) {
                    if let Some(b) = self.body_mut(s) {
                        b.mapping_count = b.mapping_count.saturating_sub(1);
                    }
                }
            }
            EdgeKind::Waiting => {
                // The object this thread was waiting on is going away, or the
                // wait is being cancelled. Either way the thread must not stay
                // Blocked with no edge to show for it (invariant I6). It goes
                // Inert with a flag, and the kernel requeues it with an error.
                if let Some(t) = self.typed::<Thread>(e.src) {
                    if let Some(b) = self.body_mut(t) {
                        if b.state == ThreadState::Blocked {
                            b.state = ThreadState::Inert;
                            b.wait_aborted = 1;
                        }
                    }
                }
            }
            EdgeKind::InSpace => {
                if let Some(t) = self.typed::<Thread>(e.src) {
                    if let Some(b) = self.body_mut(t) {
                        b.cr3 = 0;
                    }
                }
            }
            EdgeKind::Owns => {
                if let Some(h) = self.header_mut(e.dst) {
                    h.owner = NodeId::NULL;
                }
            }
            _ => {}
        }

        if let Some(h) = self.header(e.src) {
            let head = h.head(Dir::Out, kind);
            let new = list_remove(&mut self.edges, head, ei, Dir::Out);
            self.header_mut(e.src).unwrap().set_head(Dir::Out, kind, new);
        }
        if let Some(h) = self.header(e.dst) {
            let head = h.head(Dir::In, kind);
            let new = list_remove(&mut self.edges, head, ei, Dir::In);
            self.header_mut(e.dst).unwrap().set_head(Dir::In, kind, new);
        }

        self.edges.free(ei);
        self.seq += 1;
        Ok(())
    }

    #[inline]
    pub fn edge(&self, id: EdgeId) -> Option<&Edge> {
        self.edges.get(id.idx(), id.generation())
    }

    /// First out-edge of a kind. One array load: this is `pick_next`.
    #[inline]
    pub fn first_out(&self, node: NodeId, kind: EdgeKind) -> Option<EdgeId> {
        let h = self.header(node)?;
        let i = h.head(Dir::Out, kind);
        if i == 0 {
            None
        } else {
            Some(EdgeId::new(i, self.edges.at(i).generation))
        }
    }

    #[inline]
    pub fn first_in(&self, node: NodeId, kind: EdgeKind) -> Option<EdgeId> {
        let h = self.header(node)?;
        let i = h.head(Dir::In, kind);
        if i == 0 {
            None
        } else {
            Some(EdgeId::new(i, self.edges.at(i).generation))
        }
    }

    pub fn out_edges(&self, node: NodeId, kind: EdgeKind) -> EdgeIter<'_> {
        let head = self.header(node).map_or(0, |h| h.head(Dir::Out, kind));
        EdgeIter::new(&self.edges, head, Dir::Out)
    }

    pub fn in_edges(&self, node: NodeId, kind: EdgeKind) -> EdgeIter<'_> {
        let head = self.header(node).map_or(0, |h| h.head(Dir::In, kind));
        EdgeIter::new(&self.edges, head, Dir::In)
    }

    /// The owner of a node, read from the hot-hop cache rather than the edge.
    #[inline]
    pub fn owner(&self, id: NodeId) -> NodeId {
        self.header(id).map_or(NodeId::NULL, |h| h.owner)
    }

    // ------------------------------------------------- typed edge helpers ---

    /// Set a thread's address space. `InSpace` is single-valued, so this
    /// replaces any existing binding rather than adding a second one; letting
    /// two accumulate is what first broke the `cr3` cache (invariant I9).
    pub fn link_in_space(&mut self, t: Ref<Thread>, s: Ref<AddressSpace>) -> Result<EdgeId> {
        self.precheck_link(t.id, EdgeKind::InSpace, s.id)?;
        if let Some(old) = self.first_out(t.id, EdgeKind::InSpace) {
            self.unlink(old)?;
        }
        let e = self.link_raw(t.id, EdgeKind::InSpace, s.id, RawEdgeData::ZERO)?;
        let cr3 = self.body(s).map(|b| b.pml4_phys).unwrap_or(0);
        if let Some(b) = self.body_mut(t) {
            b.cr3 = cr3; // hot-hop cache (I9)
        }
        Ok(e)
    }

    pub fn link_maps(
        &mut self,
        s: Ref<AddressSpace>,
        m: Ref<MemoryObject>,
        attr: MapsAttr,
    ) -> Result<EdgeId> {
        self.precheck_link(s.id, EdgeKind::Maps, m.id)?;
        // Invariant I7: ranges within one address space must not overlap.
        for e in self.walk_out(s.id, EdgeKind::Maps) {
            if let Some(edge) = self.edge(e) {
                if MapsAttr::decode(edge.data).overlaps(&attr) {
                    return Err(GraphError::RangeOverlap);
                }
            }
        }
        let e = self.link_raw(s.id, EdgeKind::Maps, m.id, attr.encode())?;
        if let Some(b) = self.body_mut(m) {
            b.map_count += 1;
        }
        if let Some(b) = self.body_mut(s) {
            b.mapping_count += 1;
        }
        Ok(e)
    }

    pub fn link_named<B: NodeBody>(
        &mut self,
        root: Ref<Root>,
        dst: Ref<B>,
        name: &str,
    ) -> Result<EdgeId> {
        let attr = NamedAttr::new(name).ok_or(GraphError::NameTooLong)?;
        self.link_raw(root.id, EdgeKind::Named, dst.id, attr.encode())
    }

    /// The one name query the kernel offers (DESIGN appendix A).
    pub fn lookup_name(&self, name: &str) -> Option<NodeId> {
        let root = self.root()?;
        for eid in self.walk_out(root.id, EdgeKind::Named) {
            let e = self.edge(eid)?;
            if NamedAttr::decode(e.data).matches(name) {
                return Some(e.dst);
            }
        }
        None
    }

    // --------------------------------------------------- capability table ---

    /// Install a capability in the target process's handle table and return its
    /// slot. This is the only way a `Holds` edge is created, which is what
    /// makes invariant I8 hold by construction.
    pub fn grant<B: Holdable>(
        &mut self,
        holder: Ref<Process>,
        target: Ref<B>,
        rights: Rights,
    ) -> Result<u32> {
        let slot = self.find_free_slot(holder)?;
        let attr = HoldsAttr { rights, slot };
        let e = self.link_raw(holder.id, EdgeKind::Holds, target.id, attr.encode())?;
        let b = self.body_mut(holder).ok_or(GraphError::StaleNode(holder.id))?;
        b.handles[slot as usize] = e.idx();
        b.next_slot_hint = slot + 1;
        Ok(slot)
    }

    fn find_free_slot(&self, p: Ref<Process>) -> Result<u32> {
        let b = self.body(p).ok_or(GraphError::StaleNode(p.id))?;
        // Slot 0 is reserved as the null handle.
        let start = b.next_slot_hint.max(1) as usize;
        for off in 0..HANDLE_SLOTS - 1 {
            let s = 1 + (start - 1 + off) % (HANDLE_SLOTS - 1);
            if b.handles[s] == 0 {
                return Ok(s as u32);
            }
        }
        Err(GraphError::NoFreeSlot)
    }

    /// The syscall fast path: handle slot to object, with a rights check.
    /// Three dependent loads and two compares, no traversal (DESIGN 5.2).
    #[inline]
    pub fn resolve(&self, holder: Ref<Process>, slot: u32, need: Rights) -> Result<NodeId> {
        if slot == 0 || slot as usize >= HANDLE_SLOTS {
            return Err(GraphError::EmptySlot(slot));
        }
        let p: &Slot<Process> = self
            .slot(holder.id.idx(), holder.id.generation())
            .ok_or(GraphError::StaleNode(holder.id))?;
        let ei = p.body.handles[slot as usize];
        if ei == 0 {
            return Err(GraphError::EmptySlot(slot));
        }
        let e = self.edges.at(ei);
        if !e.is_live() || e.kind != EdgeKind::Holds as u8 || e.src != holder.id {
            return Err(GraphError::EmptySlot(slot));
        }
        let attr = HoldsAttr::decode(e.data);
        if !attr.rights.contains(need) {
            return Err(GraphError::MissingRights { have: attr.rights, need });
        }
        let dst = e.dst;
        let h = self.header(dst).ok_or(GraphError::StaleNode(dst))?;
        if h.is_dying() {
            return Err(GraphError::Dying(dst));
        }
        Ok(dst)
    }

    /// Rights currently held on a slot, without resolving the target.
    pub fn rights_of(&self, holder: Ref<Process>, slot: u32) -> Option<Rights> {
        let p: &Slot<Process> = self.slot(holder.id.idx(), holder.id.generation())?;
        let ei = *p.body.handles.get(slot as usize)?;
        if ei == 0 {
            return None;
        }
        let e = self.edges.at(ei);
        if !e.is_live() || e.kind != EdgeKind::Holds as u8 {
            return None;
        }
        Some(HoldsAttr::decode(e.data).rights)
    }

    pub fn revoke(&mut self, holder: Ref<Process>, slot: u32) -> Result<()> {
        let p: &Slot<Process> = self
            .slot(holder.id.idx(), holder.id.generation())
            .ok_or(GraphError::StaleNode(holder.id))?;
        let ei = *p.body.handles.get(slot as usize).ok_or(GraphError::EmptySlot(slot))?;
        if ei == 0 {
            return Err(GraphError::EmptySlot(slot));
        }
        let g = self.edges.at(ei).generation;
        self.unlink(EdgeId::new(ei, g))
    }

    /// Copy a capability into another process with reduced rights. The whole of
    /// capability transfer is one edge creation.
    pub fn copy_cap(
        &mut self,
        from: Ref<Process>,
        slot: u32,
        to: Ref<Process>,
        mask: Rights,
    ) -> Result<u32> {
        let target = self.resolve(from, slot, Rights::NONE)?;
        let have = self.rights_of(from, slot).ok_or(GraphError::EmptySlot(slot))?;
        let rights = have.intersect(mask);
        let new_slot = self.find_free_slot(to)?;
        let attr = HoldsAttr { rights, slot: new_slot };
        let e = self.link_raw(to.id, EdgeKind::Holds, target, attr.encode())?;
        let b = self.body_mut(to).ok_or(GraphError::StaleNode(to.id))?;
        b.handles[new_slot as usize] = e.idx();
        b.next_slot_hint = new_slot + 1;
        Ok(new_slot)
    }

    // ------------------------------------------------------------ threads ---

    /// Move a thread onto a cpu's run queue. One edge, one state field, both
    /// under the same lock (invariant I6).
    pub fn make_ready(&mut self, cpu: Ref<Cpu>, t: Ref<Thread>) -> Result<()> {
        let st = self.body(t).ok_or(GraphError::StaleNode(t.id))?.state;
        if st == ThreadState::Ready || st == ThreadState::Dying {
            return Err(GraphError::BadState);
        }
        // Validate before detaching: a failure here must leave the thread's
        // state and its scheduler edges agreeing (invariant I6).
        self.precheck_link(cpu.id, EdgeKind::Ready, t.id)?;
        if let Some(e) = self.waiting_edge(t) {
            self.unlink(e)?;
        }
        if let Some(e) = self.ready_edge(t) {
            self.unlink(e)?;
        }
        self.link_raw(cpu.id, EdgeKind::Ready, t.id, ReadyAttr::default().encode())?;
        self.body_mut(t).unwrap().state = ThreadState::Ready;
        Ok(())
    }

    /// Take the queue head and make it the running thread.
    pub fn make_running(&mut self, cpu: Ref<Cpu>, t: Ref<Thread>) -> Result<()> {
        if let Some(e) = self.ready_edge(t) {
            self.unlink(e)?;
        }
        self.body_mut(t).ok_or(GraphError::StaleNode(t.id))?.state = ThreadState::Running;
        self.body_mut(cpu).ok_or(GraphError::StaleNode(cpu.id))?.current = t.id;
        Ok(())
    }

    /// Block a thread on an endpoint or device.
    pub fn make_blocked<B: Waitable>(
        &mut self,
        t: Ref<Thread>,
        on: Ref<B>,
        attr: WaitingAttr,
    ) -> Result<()> {
        self.precheck_link(t.id, EdgeKind::Waiting, on.id)?;
        if let Some(e) = self.ready_edge(t) {
            self.unlink(e)?;
        }
        if let Some(e) = self.waiting_edge(t) {
            self.unlink(e)?;
        }
        self.link_raw(t.id, EdgeKind::Waiting, on.id, attr.encode())?;
        self.body_mut(t).ok_or(GraphError::StaleNode(t.id))?.state = ThreadState::Blocked;
        Ok(())
    }

    /// Normal wakeup: end a wait and queue the thread. Distinct from having the
    /// wait torn down under it, which sets `wait_aborted`.
    pub fn wake(&mut self, cpu: Ref<Cpu>, t: Ref<Thread>) -> Result<()> {
        self.precheck_link(cpu.id, EdgeKind::Ready, t.id)?;
        if let Some(e) = self.waiting_edge(t) {
            self.unlink(e)?;
        }
        if let Some(b) = self.body_mut(t) {
            b.wait_aborted = 0;
            if b.state == ThreadState::Inert {
                b.state = ThreadState::Blocked; // so make_ready accepts it
            }
        }
        self.make_ready(cpu, t)
    }

    /// The scheduler's pick-next: the head of the cpu's `Ready` list. One load.
    #[inline]
    pub fn pick_next(&self, cpu: Ref<Cpu>) -> Option<Ref<Thread>> {
        let e = self.first_out(cpu.id, EdgeKind::Ready)?;
        let dst = self.edge(e)?.dst;
        Some(Ref::from_raw(dst))
    }

    /// Round robin: move the queue head to the tail. Two splices, no allocation.
    pub fn rotate_ready(&mut self, cpu: Ref<Cpu>) -> bool {
        let head = match self.header(cpu.id) {
            Some(h) => h.head(Dir::Out, EdgeKind::Ready),
            None => return false,
        };
        if head == 0 {
            return false;
        }
        let new = list_move_to_tail(&mut self.edges, head, head, Dir::Out);
        self.header_mut(cpu.id).unwrap().set_head(Dir::Out, EdgeKind::Ready, new);
        self.seq += 1;
        true
    }

    /// The first thread waiting on an object in the given role, or none.
    /// This is IPC rendezvous: the head of one list.
    pub fn first_waiter(&self, on: NodeId, role: WaitRole) -> Option<Ref<Thread>> {
        for eid in self.walk_in(on, EdgeKind::Waiting) {
            let e = self.edge(eid)?;
            if WaitingAttr::decode(e.data).role as u8 == role as u8 {
                return Some(Ref::from_raw(e.src));
            }
        }
        None
    }

    #[inline]
    pub fn ready_edge(&self, t: Ref<Thread>) -> Option<EdgeId> {
        self.first_in(t.id, EdgeKind::Ready)
    }
    #[inline]
    pub fn waiting_edge(&self, t: Ref<Thread>) -> Option<EdgeId> {
        self.first_out(t.id, EdgeKind::Waiting)
    }
    #[inline]
    pub fn space_of(&self, t: Ref<Thread>) -> Option<Ref<AddressSpace>> {
        let e = self.first_out(t.id, EdgeKind::InSpace)?;
        Some(Ref::from_raw(self.edge(e)?.dst))
    }

    // ------------------------------------------------------- deletion ---

    /// Phase one of deletion: O(1), safe to call with interrupts disabled.
    ///
    /// From the moment this returns, the node is unreachable from the root, the
    /// scheduler will never pick it, and every handle dereference to it fails
    /// (invariant I4). Storage comes back later, in `reap_step`.
    pub fn begin_delete(&mut self, id: NodeId) -> Result<()> {
        {
            let h = self.header(id).ok_or(GraphError::StaleNode(id))?;
            if h.is_dying() {
                return Ok(());
            }
        }
        self.header_mut(id).unwrap().flags |= FLAG_DYING;

        // Detach from the ownership tree: this is what makes it unreachable.
        if let Some(e) = self.first_in(id, EdgeKind::Owns) {
            self.unlink(e)?;
        }
        // Detach from the scheduler.
        if let Some(e) = self.first_in(id, EdgeKind::Ready) {
            self.unlink(e)?;
        }
        if let Some(e) = self.first_out(id, EdgeKind::Waiting) {
            self.unlink(e)?;
        }
        if id.kind() == Some(NodeKind::Thread) {
            let r: Ref<Thread> = Ref::from_raw(id);
            if let Some(b) = self.body_mut(r) {
                b.state = ThreadState::Dying;
            }
        }

        let head = self.dying_head;
        self.header_mut(id).unwrap().next_dying = head;
        self.dying_head = id;
        self.seq += 1;
        Ok(())
    }

    /// One bounded unit of reclamation. Call from a kernel thread until it
    /// returns `Idle`; never from an interrupt handler.
    pub fn reap_step(&mut self) -> ReapStep {
        let id = self.dying_head;
        if id.is_null() {
            return ReapStep::Idle;
        }
        if self.header(id).is_none() {
            // Already gone; pop it.
            self.dying_head = NodeId::NULL;
            return ReapStep::Progress;
        }

        // Owned children die with their owner (DESIGN Q2).
        if let Some(e) = self.first_out(id, EdgeKind::Owns) {
            let child = self.edge(e).map(|e| e.dst).unwrap_or(NodeId::NULL);
            if !child.is_null() {
                let _ = self.begin_delete(child);
                return ReapStep::Progress;
            }
        }

        // Then every remaining incident edge, one per call.
        for kind in [
            EdgeKind::Holds,
            EdgeKind::InSpace,
            EdgeKind::Maps,
            EdgeKind::Ready,
            EdgeKind::Waiting,
            EdgeKind::Named,
            EdgeKind::Owns,
        ] {
            if let Some(e) = self.first_out(id, kind) {
                let _ = self.unlink(e);
                return ReapStep::Progress;
            }
            if let Some(e) = self.first_in(id, kind) {
                let waiter =
                    if kind == EdgeKind::Waiting { self.edge(e).map(|e| e.src) } else { None };
                let _ = self.unlink(e);
                if let Some(t) = waiter {
                    return ReapStep::AbortedWait { thread: t };
                }
                return ReapStep::Progress;
            }
        }

        // Isolated: release the slot.
        let next = self.header(id).unwrap().next_dying;
        let reclaim = match id.kind() {
            Some(NodeKind::MemoryObject) => {
                let r: Ref<MemoryObject> = Ref::from_raw(id);
                match self.body(r) {
                    Some(b) => {
                        Reclaim::Frames { phys: b.phys_base, pages: b.pages, flags: b.flags }
                    }
                    None => Reclaim::Nothing,
                }
            }
            Some(NodeKind::AddressSpace) => {
                let r: Ref<AddressSpace> = Ref::from_raw(id);
                match self.body(r) {
                    Some(b) if b.pml4_phys != 0 && b.is_kernel == 0 => {
                        Reclaim::PageTables { pml4_phys: b.pml4_phys }
                    }
                    _ => Reclaim::Nothing,
                }
            }
            _ => Reclaim::Nothing,
        };
        self.dying_head = next;
        if id == self.root_id {
            self.root_id = NodeId::NULL;
        }
        self.free_slot(id);
        self.seq += 1;
        ReapStep::Freed { id, reclaim }
    }

    /// Run the reaper to completion. Convenience for tests and for a quiet
    /// kernel; the kernel proper calls `reap_step` from a thread.
    pub fn reap_all(&mut self) -> u32 {
        let mut freed = 0;
        loop {
            match self.reap_step() {
                ReapStep::Idle => return freed,
                ReapStep::Freed { .. } => freed += 1,
                ReapStep::Progress | ReapStep::AbortedWait { .. } => {}
            }
        }
    }
}

// ------------------------------------------------------------------- iter ---

/// Iterates one circular adjacency list. Bounded by the arena size so that a
/// corrupted list cannot hang the kernel.
pub struct EdgeIter<'a> {
    edges: &'a EdgeSlab,
    head: u32,
    cur: u32,
    dir: Dir,
    budget: u32,
}

impl<'a> EdgeIter<'a> {
    fn new(edges: &'a EdgeSlab, head: u32, dir: Dir) -> EdgeIter<'a> {
        EdgeIter { edges, head, cur: head, dir, budget: MAX_EDGES as u32 }
    }
}

impl<'a> Iterator for EdgeIter<'a> {
    type Item = EdgeId;
    fn next(&mut self) -> Option<EdgeId> {
        if self.cur == 0 || self.budget == 0 {
            return None;
        }
        self.budget -= 1;
        let i = self.cur;
        let e = self.edges.at(i);
        let n = e.next(self.dir);
        self.cur = if n == self.head { 0 } else { n };
        Some(EdgeId::new(i, e.generation))
    }
}

/// Snapshot of an adjacency list, so a caller can walk it while mutating the
/// graph. Sized for the largest list v1 can produce.
pub const MAX_WALK: usize = HANDLE_SLOTS;

pub struct EdgeWalk {
    ids: [EdgeId; MAX_WALK],
    len: usize,
}

impl IntoIterator for EdgeWalk {
    type Item = EdgeId;
    type IntoIter = EdgeWalkIter;
    fn into_iter(self) -> EdgeWalkIter {
        EdgeWalkIter { w: self, at: 0 }
    }
}

pub struct EdgeWalkIter {
    w: EdgeWalk,
    at: usize,
}

impl Iterator for EdgeWalkIter {
    type Item = EdgeId;
    fn next(&mut self) -> Option<EdgeId> {
        if self.at >= self.w.len {
            return None;
        }
        let v = self.w.ids[self.at];
        self.at += 1;
        Some(v)
    }
}

// ------------------------------------------------------- walks and cursors ---

impl Graph {
    /// Copy an adjacency list into a fixed buffer so a caller can iterate it
    /// while mutating the graph. Bounded by `MAX_WALK`; nothing on a fast path
    /// uses this.
    pub fn walk_out(&self, node: NodeId, kind: EdgeKind) -> EdgeWalk {
        Self::walk(self.out_edges(node, kind))
    }

    pub fn walk_in(&self, node: NodeId, kind: EdgeKind) -> EdgeWalk {
        Self::walk(self.in_edges(node, kind))
    }

    fn walk(it: EdgeIter<'_>) -> EdgeWalk {
        let mut w = EdgeWalk { ids: [EdgeId::NULL; MAX_WALK], len: 0 };
        for id in it {
            if w.len == MAX_WALK {
                break;
            }
            w.ids[w.len] = id;
            w.len += 1;
        }
        w
    }

    /// Every live node, in arena order. Checker and snapshot only.
    pub fn live_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        let roots = self
            .roots
            .live_indices()
            .map(move |i| NodeId::new(NodeKind::Root, i, self.roots.slots[i as usize].generation));
        let cpus = self
            .cpus
            .live_indices()
            .map(move |i| NodeId::new(NodeKind::Cpu, i, self.cpus.slots[i as usize].generation));
        let procs = self.processes.live_indices().map(move |i| {
            NodeId::new(NodeKind::Process, i, self.processes.slots[i as usize].generation)
        });
        let threads = self.threads.live_indices().map(move |i| {
            NodeId::new(NodeKind::Thread, i, self.threads.slots[i as usize].generation)
        });
        let spaces = self.spaces.live_indices().map(move |i| {
            NodeId::new(NodeKind::AddressSpace, i, self.spaces.slots[i as usize].generation)
        });
        let mems = self.memobjs.live_indices().map(move |i| {
            NodeId::new(NodeKind::MemoryObject, i, self.memobjs.slots[i as usize].generation)
        });
        let eps = self.endpoints.live_indices().map(move |i| {
            NodeId::new(NodeKind::Endpoint, i, self.endpoints.slots[i as usize].generation)
        });
        let devs = self.devices.live_indices().map(move |i| {
            NodeId::new(NodeKind::Device, i, self.devices.slots[i as usize].generation)
        });
        roots.chain(cpus).chain(procs).chain(threads).chain(spaces).chain(mems).chain(eps).chain(devs)
    }

    /// Every live edge id.
    pub fn live_edges(&self) -> impl Iterator<Item = EdgeId> + '_ {
        self.edges.live_indices().map(move |i| EdgeId::new(i, self.edges.at(i).generation))
    }
}

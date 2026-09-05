//! Where the graph meets the hardware.
//!
//! Invariant I5 says page tables are a cache of `Maps` edges. This module is
//! the whole of that invariant's enforcement: `map` and `unmap` are the only
//! functions in the kernel that write a user page-table entry, and they create
//! or destroy the corresponding edge in the same operation. `check` walks the
//! tables and diffs them against the edges in both directions.
//!
//! Lock order throughout the kernel is `GRAPH`, then `BOOT`, then `FRAMES`.
//! Taking them in any other order would deadlock, and every site here follows
//! it.

use bramble_graph::body::*;
use bramble_graph::edge::{EdgeAttr, MapFlags, MapsAttr, Prot};
use bramble_graph::graph::{Graph, GraphError, Ref};

use bramble_graph::id::{EdgeId, EdgeKind, NodeId, NodeKind};

use crate::paging::{self, MapError, PAGE_SIZE};
use crate::state::{FRAMES, GRAPH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmError {
    Graph(GraphError),
    Paging(MapError),
    /// The requested slice runs off the end of the memory object.
    OutOfBounds,
    NoAllocator,
    StaleSpace,
}

impl From<GraphError> for VmError {
    fn from(e: GraphError) -> VmError {
        VmError::Graph(e)
    }
}

/// Create an address space with an empty user half, owned by the root.
pub fn create_space(owner: Ref<Root>) -> Result<Ref<AddressSpace>, VmError> {
    let mut g = GRAPH.lock();
    let mut fa = FRAMES.lock();
    let fa = fa.as_mut().ok_or(VmError::NoAllocator)?;
    let pml4 = paging::new_address_space(fa).ok_or(VmError::Paging(MapError::OutOfFrames))?;
    match g.create_under_root(owner, AddressSpace { pml4_phys: pml4, ..AddressSpace::ZERO }) {
        Ok(s) => Ok(s),
        Err(e) => {
            // SAFETY: nothing is running on a space we just created.
            unsafe { paging::free_user_tables(pml4, fa) };
            Err(VmError::Graph(e))
        }
    }
}

/// The same, owned by a process, so the space dies with it.
pub fn create_space_for(owner: Ref<Process>) -> Result<Ref<AddressSpace>, VmError> {
    let mut g = GRAPH.lock();
    let mut fa = FRAMES.lock();
    let fa = fa.as_mut().ok_or(VmError::NoAllocator)?;
    let pml4 = paging::new_address_space(fa).ok_or(VmError::Paging(MapError::OutOfFrames))?;
    match g.create_under_process(owner, AddressSpace { pml4_phys: pml4, ..AddressSpace::ZERO }) {
        Ok(s) => Ok(s),
        Err(e) => {
            // SAFETY: nothing is running on a space we just created.
            unsafe { paging::free_user_tables(pml4, fa) };
            Err(VmError::Graph(e))
        }
    }
}

/// Allocate frames described by a node owned by a process.
pub fn alloc_object_for(owner: Ref<Process>, pages: u32) -> Result<Ref<MemoryObject>, VmError> {
    let mut g = GRAPH.lock();
    let mut fa = FRAMES.lock();
    let fa = fa.as_mut().ok_or(VmError::NoAllocator)?;
    let phys = fa
        .alloc_contiguous(pages as usize)
        .ok_or(VmError::Paging(MapError::OutOfFrames))?;
    match g.create_under_process(
        owner,
        MemoryObject { phys_base: phys, pages, flags: MemFlags::NONE, ..MemoryObject::ZERO },
    ) {
        Ok(m) => Ok(m),
        Err(e) => {
            fa.free_contiguous(phys, pages as usize);
            Err(VmError::Graph(e))
        }
    }
}

/// Allocate physically contiguous frames and describe them with a node. The
/// node is what makes the frames accountable: from here on the reaper knows to
/// return them (DESIGN 3.7).
pub fn alloc_object(owner: Ref<Root>, pages: u32) -> Result<Ref<MemoryObject>, VmError> {
    let mut g = GRAPH.lock();
    let mut fa = FRAMES.lock();
    let fa = fa.as_mut().ok_or(VmError::NoAllocator)?;
    let phys = fa
        .alloc_contiguous(pages as usize)
        .ok_or(VmError::Paging(MapError::OutOfFrames))?;
    match g.create_under_root(
        owner,
        MemoryObject { phys_base: phys, pages, flags: MemFlags::NONE, ..MemoryObject::ZERO },
    ) {
        Ok(m) => Ok(m),
        Err(e) => {
            fa.free_contiguous(phys, pages as usize);
            Err(VmError::Graph(e))
        }
    }
}

/// Map a slice of a memory object into an address space.
///
/// The edge comes first, because creating it is what performs the overlap check
/// (invariant I7). If the page-table write then fails, the edge is removed
/// again: a `Maps` edge with no page tables behind it is precisely the state
/// I5 forbids.
pub fn map(
    space: Ref<AddressSpace>,
    obj: Ref<MemoryObject>,
    attr: MapsAttr,
) -> Result<EdgeId, VmError> {
    let mut g = GRAPH.lock();

    let (phys_base, pages) = {
        let b = g.body(obj).ok_or(VmError::StaleSpace)?;
        (b.phys_base, b.pages)
    };
    if attr.off_pages + attr.len_pages > pages {
        return Err(VmError::OutOfBounds);
    }
    let pml4 = g.body(space).ok_or(VmError::StaleSpace)?.pml4_phys;

    let edge = g.link_maps(space, obj, attr)?;

    let paddr = phys_base + attr.off_pages as u64 * PAGE_SIZE;
    let mut fa = FRAMES.lock();
    let fa = match fa.as_mut() {
        Some(fa) => fa,
        None => {
            let _ = g.unlink(edge);
            return Err(VmError::NoAllocator);
        }
    };
    // A lazy mapping writes no entries at all. The edge is the mapping; the
    // page tables are a cache of it (invariant I5), and a cache is allowed to
    // be cold. `fault_in` fills it a page at a time.
    if attr.flags.contains(MapFlags::LAZY) {
        return Ok(edge);
    }

    // SAFETY: `pml4` came from an AddressSpace node this kernel created, and
    // the range was just checked not to overlap an existing mapping.
    let r = unsafe { paging::map_pages(pml4, attr.vaddr, paddr, attr.len_pages, attr.prot, fa) };
    if let Err(e) = r {
        // Roll back both halves, in the order that leaves nothing dangling.
        unsafe { paging::unmap_pages(pml4, attr.vaddr, attr.len_pages) };
        let _ = g.unlink(edge);
        return Err(VmError::Paging(e));
    }
    Ok(edge)
}

/// Remove a mapping: clear the entries, flush, then drop the edge.
pub fn unmap(edge: EdgeId) -> Result<(), VmError> {
    let mut g = GRAPH.lock();
    let (pml4, attr) = {
        let e = g.edge(edge).ok_or(VmError::Graph(GraphError::StaleEdge(edge)))?;
        let attr = MapsAttr::decode(e.data);
        let space: Ref<AddressSpace> = g.typed(e.src).ok_or(VmError::StaleSpace)?;
        (g.body(space).ok_or(VmError::StaleSpace)?.pml4_phys, attr)
    };
    // SAFETY: `pml4` belongs to a live AddressSpace node.
    unsafe { paging::unmap_pages(pml4, attr.vaddr, attr.len_pages) };
    g.unlink(edge)?;
    Ok(())
}

/// How many faults have been served by filling in a lazy mapping.
pub static LAZY_FAULTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Cycles spent inside `find_mapping`, and how many lookups that covers.
pub static LOOKUP_CYCLES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Try to satisfy a page fault by filling in one page of a lazy mapping.
///
/// This is the question DESIGN 5.3 admits the graph cannot answer on its own:
/// *which mapping covers this address?* It is a range query, and adjacency
/// lists relate objects rather than intervals. The answer is the range index —
/// a binary search over the space's `Maps` edges, kept sorted by the same two
/// operations that create and destroy them.
///
/// Returns false if nothing here authorises the access, in which case the
/// caller destroys the process.
pub fn fault_in(addr: u64) -> bool {
    use core::sync::atomic::Ordering;
    let page = addr & !(PAGE_SIZE - 1);
    let (pml4, paddr, prot) = {
        let g = GRAPH.lock();
        let t: Ref<Thread> = match g.typed(crate::sched::current_locked(&g)) {
            Some(t) => t,
            None => return false,
        };
        let space = match g.space_of(t) {
            Some(s) => s,
            None => return false,
        };

        let start = crate::time::rdtsc();
        let found = g.find_mapping(space, page);
        LOOKUP_CYCLES.fetch_add(crate::time::rdtsc() - start, Ordering::Relaxed);

        let (eid, attr) = match found {
            Some(x) => x,
            None => return false, // nothing is mapped here at all
        };
        if !attr.flags.contains(MapFlags::LAZY) {
            // An eager mapping is already in the tables, so a fault on one is a
            // protection violation and the process's own doing.
            return false;
        }
        let obj_id = match g.edge(eid) {
            Some(e) => e.dst,
            None => return false,
        };
        let obj: Ref<MemoryObject> = match g.typed(obj_id) {
            Some(o) => o,
            None => return false,
        };
        let base = match g.body(obj) {
            Some(b) => b.phys_base,
            None => return false,
        };
        let index = (page - attr.vaddr) / PAGE_SIZE + attr.off_pages as u64;
        let pml4 = match g.body(space) {
            Some(b) => b.pml4_phys,
            None => return false,
        };
        (pml4, base + index * PAGE_SIZE, attr.prot)
    };

    let mut fa = FRAMES.lock();
    let fa = match fa.as_mut() {
        Some(f) => f,
        None => return false,
    };
    // SAFETY: a page-table root this kernel built, and one page inside a range
    // the graph says this address space may use.
    match unsafe { paging::map_pages(pml4, page, paddr, 1, prot, fa) } {
        Ok(()) => {
            LAZY_FAULTS.fetch_add(1, Ordering::Relaxed);
            true
        }
        // Already present means the fault was about permissions, not absence.
        Err(_) => false,
    }
}

// ------------------------------------------------------------- invariant ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum I5 {
    /// The graph says this page is mapped; the hardware disagrees.
    MissingEntry { space: NodeId, edge: EdgeId, vaddr: u64 },
    /// Mapped, but to the wrong frame.
    WrongFrame { space: NodeId, edge: EdgeId, vaddr: u64, want: u64, got: u64 },
    /// Mapped to the right frame with the wrong permissions.
    WrongProt { space: NodeId, edge: EdgeId, vaddr: u64, entry: u64, prot: Prot },
    /// The hardware has a mapping the graph never authorised. This is the
    /// direction that catches a leak of authority rather than a loss of it.
    UnknownEntry { space: NodeId, vaddr: u64, phys: u64 },
}

/// Verify invariant I5 for every user address space, in both directions.
///
/// The kernel's own space is exempt (I5'): it is one fixed mapping of all of
/// physical memory plus the image, and walking it would be O(RAM) and prove
/// nothing.
pub fn check_page_tables(g: &Graph) -> Result<(), I5> {
    for node in g.live_nodes() {
        if node.kind() != Some(NodeKind::AddressSpace) {
            continue;
        }
        let space: Ref<AddressSpace> = g.typed(node).expect("address space");
        let body = g.body(space).expect("body");
        if body.is_kernel != 0 {
            continue;
        }
        let pml4 = body.pml4_phys;

        // Direction one: every edge is backed by the entries it claims.
        for eid in g.out_edges(node, EdgeKind::Maps) {
            let edge = g.edge(eid).expect("live edge");
            let attr = MapsAttr::decode(edge.data);
            let obj: Ref<MemoryObject> = g.typed(edge.dst).expect("memory object");
            let base = g.body(obj).expect("body").phys_base;
            for i in 0..attr.len_pages as u64 {
                let vaddr = attr.vaddr + i * PAGE_SIZE;
                let want = base + (attr.off_pages as u64 + i) * PAGE_SIZE;
                // SAFETY: reading page tables of a space this kernel owns.
                match unsafe { paging::translate(pml4, vaddr) } {
                    // A lazy mapping is allowed to have no entry yet: the edge
                    // is the mapping and the tables are a cache of it, so a cold
                    // cache is not a divergence. What is *not* allowed is an
                    // entry nobody authorised, and the second direction below
                    // still checks that with no exception at all.
                    None if attr.flags.contains(MapFlags::LAZY) => continue,
                    None => return Err(I5::MissingEntry { space: node, edge: eid, vaddr }),
                    Some((got, entry)) => {
                        if got != want {
                            return Err(I5::WrongFrame {
                                space: node,
                                edge: eid,
                                vaddr,
                                want,
                                got,
                            });
                        }
                        if !paging::flags_match(entry, attr.prot) {
                            return Err(I5::WrongProt {
                                space: node,
                                edge: eid,
                                vaddr,
                                entry,
                                prot: attr.prot,
                            });
                        }
                    }
                }
            }
        }

        // Direction two: no entry exists that no edge asked for.
        let mut bad = None;
        // SAFETY: reading page tables of a space this kernel owns.
        unsafe {
            paging::walk_user_pages(pml4, &mut |vaddr, entry| {
                let phys = entry & 0x000F_FFFF_FFFF_F000;
                if !covered(g, node, vaddr, phys) {
                    bad = Some(I5::UnknownEntry { space: node, vaddr, phys });
                    return false;
                }
                true
            })
        };
        if let Some(v) = bad {
            return Err(v);
        }
    }
    Ok(())
}

/// Is this hardware mapping accounted for by some `Maps` edge on this space?
fn covered(g: &Graph, space: NodeId, vaddr: u64, phys: u64) -> bool {
    for eid in g.out_edges(space, EdgeKind::Maps) {
        let edge = match g.edge(eid) {
            Some(e) => e,
            None => continue,
        };
        let attr = MapsAttr::decode(edge.data);
        if vaddr < attr.vaddr || vaddr >= attr.end() {
            continue;
        }
        let obj: Ref<MemoryObject> = match g.typed(edge.dst) {
            Some(o) => o,
            None => continue,
        };
        let base = match g.body(obj) {
            Some(b) => b.phys_base,
            None => continue,
        };
        let page = (vaddr - attr.vaddr) / PAGE_SIZE;
        if base + (attr.off_pages as u64 + page) * PAGE_SIZE == phys {
            return true;
        }
    }
    false
}

/// Check I5 against the live graph, taking the lock.
pub fn check_now() -> Result<(), I5> {
    let g = GRAPH.lock();
    check_page_tables(&g)
}

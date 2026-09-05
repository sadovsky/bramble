//! Unit and property tests. The checker runs after *every* operation in the
//! randomised tests, which is the point: the graph's job is to be verifiable.

use crate::body::*;
use crate::checker::Checker;
use crate::edge::*;
use crate::graph::*;
use crate::id::*;
use crate::limits::*;

use std::boxed::Box;
use std::vec::Vec;

fn empty() -> Box<Graph> {
    Box::new(Graph::EMPTY)
}

fn check(g: &Graph) {
    let mut c = Checker::new();
    if let Err(v) = c.check(g) {
        panic!("invariant violation: {:?}", v);
    }
}

// ------------------------------------------------------------ foundations ---

#[test]
fn zeroed_memory_is_a_valid_empty_graph() {
    // This is the property that lets the graph live in `.bss` and exist before
    // the allocator does (DESIGN 3.7).
    let g = empty();
    assert_eq!(g.node_count(), 0);
    assert_eq!(g.edge_count(), 0);
    assert!(g.root().is_none());
    assert!(!g.has_pending_reap());
    check(&g);
}

#[test]
fn report_sizes() {
    use core::mem::size_of;
    std::println!("Edge            {:>8} bytes", size_of::<Edge>());
    std::println!("NodeHeader      {:>8} bytes", size_of::<crate::slab::NodeHeader>());
    std::println!("NodeId          {:>8} bytes", size_of::<NodeId>());
    std::println!("Thread body     {:>8} bytes", size_of::<Thread>());
    std::println!("Process body    {:>8} bytes", size_of::<Process>());
    std::println!("Graph total     {:>8} bytes ({} KiB)", size_of::<Graph>(), size_of::<Graph>() / 1024);
    assert_eq!(size_of::<Edge>(), 64);
    assert_eq!(size_of::<NodeId>(), 8);
    assert_eq!(size_of::<EdgeId>(), 8);
}

#[test]
fn stale_ids_never_resolve() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    let stale = p.id();
    assert!(g.is_live(stale));

    g.begin_delete(stale).unwrap();
    g.reap_all();

    assert!(!g.is_live(stale));
    assert!(g.header(stale).is_none());
    assert!(g.typed::<Process>(stale).is_none());

    // Reuse the slot; the old id must still fail even though the index matches.
    let p2 = g.create_under_root(root, Process::ZERO).unwrap();
    assert_eq!(p2.id().idx(), stale.idx(), "slot should have been reused");
    assert_ne!(p2.id().generation(), stale.generation());
    assert!(g.header(stale).is_none());
    check(&g);
}

#[test]
fn null_and_forged_ids_are_rejected() {
    let g = empty();
    assert!(g.header(NodeId::NULL).is_none());
    // Even generations are never handed out, so a forged even generation fails.
    assert!(g.header(NodeId::new(NodeKind::Process, 0, 2)).is_none());
    assert!(g.header(NodeId::new(NodeKind::Process, 999999, 1)).is_none());
}

// ------------------------------------------------------------- adjacency ---

#[test]
fn ready_queue_is_fifo_and_rotates() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let cpu = g.create_under_root(root, Cpu::ZERO).unwrap();
    let proc = g.create_under_root(root, Process::ZERO).unwrap();

    let ts: Vec<_> = (0..4)
        .map(|_| g.create_under_process(proc, Thread::ZERO).unwrap())
        .collect();
    for t in &ts {
        g.make_ready(cpu, *t).unwrap();
    }
    check(&g);

    // Queue order is list order.
    assert_eq!(g.pick_next(cpu).unwrap(), ts[0]);
    g.rotate_ready(cpu);
    assert_eq!(g.pick_next(cpu).unwrap(), ts[1]);
    g.rotate_ready(cpu);
    assert_eq!(g.pick_next(cpu).unwrap(), ts[2]);
    g.rotate_ready(cpu);
    g.rotate_ready(cpu);
    assert_eq!(g.pick_next(cpu).unwrap(), ts[0], "round robin wraps");
    check(&g);

    // Removing the head leaves the rest intact.
    g.make_running(cpu, ts[0]).unwrap();
    assert_eq!(g.pick_next(cpu).unwrap(), ts[1]);
    check(&g);
}

#[test]
fn typed_adjacency_is_independent_per_kind() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let proc = g.create_under_root(root, Process::ZERO).unwrap();
    let ep = g.create_under_process(proc, Endpoint::ZERO).unwrap();
    g.grant(proc, ep, Rights::ALL).unwrap();

    // The process has one Owns out-edge to the endpoint and one Holds out-edge
    // to the same endpoint; the two lists do not interfere.
    assert_eq!(g.out_edges(proc.id(), EdgeKind::Owns).count(), 1);
    assert_eq!(g.out_edges(proc.id(), EdgeKind::Holds).count(), 1);
    assert_eq!(g.in_edges(ep.id(), EdgeKind::Owns).count(), 1);
    assert_eq!(g.in_edges(ep.id(), EdgeKind::Holds).count(), 1);
    check(&g);
}

// ---------------------------------------------------------- capabilities ---

#[test]
fn resolve_enforces_rights_and_liveness() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let proc = g.create_under_root(root, Process::ZERO).unwrap();
    let ep = g.create_under_process(proc, Endpoint::ZERO).unwrap();

    let slot = g.grant(proc, ep, Rights::SEND).unwrap();
    assert_eq!(g.resolve(proc, slot, Rights::SEND).unwrap(), ep.id());
    assert!(matches!(
        g.resolve(proc, slot, Rights::RECV),
        Err(GraphError::MissingRights { .. })
    ));
    assert!(matches!(g.resolve(proc, 0, Rights::NONE), Err(GraphError::EmptySlot(0))));
    assert!(matches!(g.resolve(proc, 200, Rights::NONE), Err(GraphError::EmptySlot(200))));

    // Authority is one hop: a dying target stops resolving immediately, before
    // any storage has been reclaimed (invariant I4).
    g.begin_delete(ep.id()).unwrap();
    assert!(matches!(g.resolve(proc, slot, Rights::SEND), Err(GraphError::Dying(_))));
    check(&g);
}

#[test]
fn capability_transfer_can_only_narrow_rights() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let a = g.create_under_root(root, Process::ZERO).unwrap();
    let b = g.create_under_root(root, Process::ZERO).unwrap();
    let ep = g.create_under_process(a, Endpoint::ZERO).unwrap();

    let s = g.grant(a, ep, Rights::SEND.union(Rights::RECV)).unwrap();
    let s2 = g.copy_cap(a, s, b, Rights::SEND).unwrap();

    assert_eq!(g.rights_of(b, s2).unwrap(), Rights::SEND);
    assert!(g.resolve(b, s2, Rights::RECV).is_err());
    // Asking for more than the holder has cannot widen it.
    let s3 = g.copy_cap(b, s2, a, Rights::ALL).unwrap();
    assert_eq!(g.rights_of(a, s3).unwrap(), Rights::SEND);
    check(&g);
}

#[test]
fn revocation_is_a_list_walk_not_a_search() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let owner = g.create_under_root(root, Process::ZERO).unwrap();
    let ep = g.create_under_process(owner, Endpoint::ZERO).unwrap();

    // Ten processes hold a capability to the same endpoint.
    let holders: Vec<_> = (0..10)
        .map(|_| {
            let p = g.create_under_root(root, Process::ZERO).unwrap();
            let s = g.grant(p, ep, Rights::SEND).unwrap();
            (p, s)
        })
        .collect();
    assert_eq!(g.in_edges(ep.id(), EdgeKind::Holds).count(), 10);
    check(&g);

    // Destroying the endpoint revokes all ten. seL4 needs a capability
    // derivation tree for this; here it is the in-list.
    g.begin_delete(ep.id()).unwrap();
    g.reap_all();
    for (p, s) in holders {
        assert!(g.resolve(p, s, Rights::SEND).is_err());
        assert!(g.rights_of(p, s).is_none(), "handle slot must be cleared too");
    }
    check(&g);
}

// -------------------------------------------------------------- lifetime ---

#[test]
fn deleting_an_owner_destroys_its_subtree() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let base_nodes = g.node_count();
    let base_edges = g.edge_count();

    let parent = g.create_under_root(root, Process::ZERO).unwrap();
    let space = g.create_under_process(parent, AddressSpace::ZERO).unwrap();
    let child = g.create_under_process(parent, Process::ZERO).unwrap();
    let gchild = g.create_under_process(child, Thread::ZERO).unwrap();
    let mem = g.create_under_process(child, MemoryObject { phys_base: 0x1000, pages: 4, ..MemoryObject::ZERO }).unwrap();
    g.link_maps(space, mem, MapsAttr { vaddr: 0x400000, len_pages: 4, off_pages: 0, prot: Prot::READ, flags: MapFlags::NONE }).unwrap();
    g.grant(parent, mem, Rights::READ).unwrap();
    check(&g);

    let ids = [parent.id(), space.id(), child.id(), gchild.id(), mem.id()];
    g.begin_delete(parent.id()).unwrap();

    // Phase one is instantaneous unreachability, even though storage is still held.
    assert!(g.is_dying(parent.id()));
    assert!(g.owner(parent.id()).is_null());

    g.reap_all();
    for id in ids {
        assert!(!g.is_live(id), "{:?} should be gone", id);
    }
    // Reachability-based cleanup with no counting and no tracing.
    assert_eq!(g.node_count(), base_nodes);
    assert_eq!(g.edge_count(), base_edges);
    check(&g);
}

#[test]
fn reaping_is_bounded_per_step() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    for _ in 0..64 {
        let m = g.create_under_process(p, MemoryObject::ZERO).unwrap();
        g.grant(p, m, Rights::READ).unwrap();
    }
    check(&g);

    g.begin_delete(p.id()).unwrap();
    // Every step does O(1) work, so a large subtree never blocks interrupts.
    let mut steps = 0;
    while !matches!(g.reap_step(), ReapStep::Idle) {
        steps += 1;
        assert!(steps < 10_000, "reaper made no progress");
        if steps % 7 == 0 {
            check(&g); // the graph is consistent between every pair of steps
        }
    }
    assert!(steps > 64, "expected many bounded steps, got {}", steps);
    assert_eq!(g.node_count(), 1, "only the root should remain");
    check(&g);
}

#[test]
fn memory_objects_report_their_frames_when_reaped() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    g.create_under_process(p, MemoryObject { phys_base: 0xdead_0000, pages: 7, ..MemoryObject::ZERO })
        .unwrap();

    g.begin_delete(p.id()).unwrap();
    let mut reclaimed = Vec::new();
    loop {
        match g.reap_step() {
            ReapStep::Idle => break,
            ReapStep::Freed { reclaim: Reclaim::Frames { phys, pages, flags }, .. } => {
                reclaimed.push((phys, pages, flags))
            }
            _ => {}
        }
    }
    assert_eq!(reclaimed, [(0xdead_0000u64, 7u32, MemFlags::NONE)]);
}

// ------------------------------------------------------------ invariants ---

#[test]
fn incompatible_edges_are_rejected_at_runtime() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    let t = g.create_under_process(p, Thread::ZERO).unwrap();
    // A thread cannot own anything.
    assert!(matches!(
        g.link_raw(t.id(), EdgeKind::Owns, p.id(), RawEdgeData::ZERO),
        Err(GraphError::Incompatible { .. })
    ));
    // A process is not a run queue.
    assert!(matches!(
        g.link_raw(p.id(), EdgeKind::Ready, t.id(), RawEdgeData::ZERO),
        Err(GraphError::Incompatible { .. })
    ));
    check(&g);
}

#[test]
fn overlapping_mappings_are_rejected() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    let s = g.create_under_process(p, AddressSpace::ZERO).unwrap();
    let m = g.create_under_process(p, MemoryObject { pages: 8, ..MemoryObject::ZERO }).unwrap();

    let a = MapsAttr { vaddr: 0x1000_0000, len_pages: 4, off_pages: 0, prot: Prot::READ, flags: MapFlags::NONE };
    g.link_maps(s, m, a).unwrap();
    let overlapping = MapsAttr { vaddr: 0x1000_2000, len_pages: 4, off_pages: 0, prot: Prot::READ, flags: MapFlags::NONE };
    assert!(matches!(g.link_maps(s, m, overlapping), Err(GraphError::RangeOverlap)));
    let adjacent = MapsAttr { vaddr: 0x1000_4000, len_pages: 4, off_pages: 4, prot: Prot::READ, flags: MapFlags::NONE };
    g.link_maps(s, m, adjacent).unwrap();
    assert_eq!(g.body(m).unwrap().map_count, 2);
    check(&g);
}

#[test]
fn cr3_cache_tracks_the_in_space_edge() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    let s = g.create_under_process(p, AddressSpace { pml4_phys: 0x2000, ..AddressSpace::ZERO }).unwrap();
    let t = g.create_under_process(p, Thread::ZERO).unwrap();

    assert_eq!(g.body(t).unwrap().cr3, 0);
    let e = g.link_in_space(t, s).unwrap();
    assert_eq!(g.body(t).unwrap().cr3, 0x2000, "hot-hop cache follows the edge");
    check(&g);
    g.unlink(e).unwrap();
    assert_eq!(g.body(t).unwrap().cr3, 0);
    check(&g);
}

#[test]
fn names_are_edges_not_a_hierarchy() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let dev = g.create_under_root(root, Device { class: DeviceClass::SerialConsole, io_base: 0x3f8, ..Device::ZERO }).unwrap();

    g.link_named(root, dev, "console").unwrap();
    g.link_named(root, dev, "tty0").unwrap();
    assert_eq!(g.lookup_name("console"), Some(dev.id()));
    assert_eq!(g.lookup_name("tty0"), Some(dev.id()), "aliases are free");
    assert_eq!(g.lookup_name("nope"), None);
    assert!(g.link_named(root, dev, "a-name-that-is-far-too-long-to-fit").is_err());
    check(&g);
}

#[test]
fn arena_exhaustion_is_an_error_not_a_panic() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let mut made = 0;
    loop {
        match g.create_under_root(root, Device::ZERO) {
            Ok(_) => made += 1,
            Err(GraphError::ArenaFull(NodeKind::Device)) => break,
            Err(e) => panic!("unexpected {:?}", e),
        }
        assert!(made <= MAX_DEVICES);
    }
    assert_eq!(made, MAX_DEVICES);
    check(&g);
}

#[test]
fn a_full_handle_table_reports_no_free_slot() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    let ep = g.create_under_process(p, Endpoint::ZERO).unwrap();
    for _ in 0..HANDLE_SLOTS - 1 {
        g.grant(p, ep, Rights::SEND).unwrap();
    }
    assert!(matches!(g.grant(p, ep, Rights::SEND), Err(GraphError::NoFreeSlot)));
    check(&g);
}

// -------------------------------------------------------- property tests ---

/// xorshift64*, so the random tests are deterministic and reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> Option<T> {
        if v.is_empty() {
            None
        } else {
            Some(v[self.below(v.len())])
        }
    }
}

/// The central property: after any sequence of operations, valid or rejected,
/// every invariant still holds.
fn random_ops(seed: u64, steps: usize, check_every: usize) {
    let mut rng = Rng(seed);
    let mut g = empty();
    let mut chk = Checker::new();
    let root = g.create_root().unwrap();
    let cpu = g.create_under_root(root, Cpu::ZERO).unwrap();

    let mut procs: Vec<Ref<Process>> = Vec::new();
    let mut threads: Vec<Ref<Thread>> = Vec::new();
    let mut spaces: Vec<Ref<AddressSpace>> = Vec::new();
    let mut mems: Vec<Ref<MemoryObject>> = Vec::new();
    let mut eps: Vec<Ref<Endpoint>> = Vec::new();
    let mut caps: Vec<(Ref<Process>, u32)> = Vec::new();
    let mut next_vaddr: u64;
    let mut history: Vec<(usize, usize, NodeId)> = Vec::new();
    let note = |h: &mut Vec<(usize, usize, NodeId)>, step, op, id: NodeId| {
        h.push((step, op, id));
        if h.len() > 40 {
            h.remove(0);
        }
    };

    for step in 0..steps {
        let op = rng.below(14);
        match op {
            0 => {
                if let Ok(p) = g.create_under_root(root, Process::ZERO) {
                    procs.push(p);
                }
            }
            1 => {
                if let Some(p) = rng.pick(&procs) {
                    if let Ok(t) = g.create_under_process(p, Thread::ZERO) {
                        threads.push(t);
                    }
                }
            }
            2 => {
                if let Some(p) = rng.pick(&procs) {
                    let pml4 = 0x1000 + (rng.next() & 0xFF) * 0x1000;
                    if let Ok(s) =
                        g.create_under_process(p, AddressSpace { pml4_phys: pml4, ..AddressSpace::ZERO })
                    {
                        spaces.push(s);
                    }
                }
            }
            3 => {
                // Half the time hang the memory off a thread instead of a
                // process, so the thread-owns-its-stack edge is exercised.
                let body =
                    MemoryObject { phys_base: rng.next() & !0xFFF, pages: 4, ..MemoryObject::ZERO };
                let made = if rng.next() & 1 == 0 {
                    rng.pick(&threads).and_then(|t| g.create_under_thread(t, body).ok())
                } else {
                    rng.pick(&procs).and_then(|p| g.create_under_process(p, body).ok())
                };
                if let Some(m) = made {
                    mems.push(m);
                }
            }
            4 => {
                if let Some(p) = rng.pick(&procs) {
                    if let Ok(e) = g.create_under_process(p, Endpoint::ZERO) {
                        eps.push(e);
                    }
                }
            }
            5 => {
                if let (Some(t), Some(s)) = (rng.pick(&threads), rng.pick(&spaces)) {
                    let _ = g.link_in_space(t, s);
                }
            }
            6 => {
                if let (Some(s), Some(m)) = (rng.pick(&spaces), rng.pick(&mems)) {
                    // Scattered rather than rising, so the index has to sort,
                    // and often colliding, so the overlap check is exercised.
                    next_vaddr = 0x1000_0000 + (rng.next() % 64) * 0x10_0000;
                    let _ = g.link_maps(
                        s,
                        m,
                        MapsAttr { vaddr: next_vaddr, len_pages: 4, off_pages: 0, prot: Prot::READ, flags: MapFlags::NONE },
                    );
                }
            }
            7 => {
                if let (Some(p), Some(e)) = (rng.pick(&procs), rng.pick(&eps)) {
                    if let Ok(slot) = g.grant(p, e, Rights::SEND.union(Rights::RECV)) {
                        caps.push((p, slot));
                    }
                }
            }
            8 => {
                if let Some((p, slot)) = rng.pick(&caps) {
                    let _ = g.revoke(p, slot);
                    caps.retain(|&(cp, cs)| !(cp == p && cs == slot));
                }
            }
            9 => {
                if let Some(t) = rng.pick(&threads) {
                    note(&mut history, step, op, t.id());
                    let _ = g.make_ready(cpu, t);
                }
            }
            10 => {
                if let (Some(t), Some(e)) = (rng.pick(&threads), rng.pick(&eps)) {
                    note(&mut history, step, op, t.id());
                    let _ = g.make_blocked(t, e, WaitingAttr { role: WaitRole::Recv, badge: 1 });
                }
            }
            11 if rng.next().is_multiple_of(4) => {
                // Unmap something, so the index is exercised in both directions.
                if let Some(s) = rng.pick(&spaces) {
                    let victim = g.walk_out(s.id(), EdgeKind::Maps).into_iter().next();
                    if let Some(e) = victim {
                        let _ = g.unlink(e);
                    }
                }
            }
            11 => {
                g.rotate_ready(cpu);
                if let Some(t) = g.pick_next(cpu) {
                    note(&mut history, step, op, t.id());
                    let _ = g.make_running(cpu, t);
                }
            }
            12 => {
                // Delete something at random. Never the root or the cpu.
                let victims: Vec<NodeId> = procs
                    .iter()
                    .map(|r| r.id())
                    .chain(threads.iter().map(|r| r.id()))
                    .chain(mems.iter().map(|r| r.id()))
                    .chain(eps.iter().map(|r| r.id()))
                    .filter(|id| g.is_live(*id) && !g.is_dying(*id))
                    .collect();
                if let Some(v) = rng.pick(&victims) {
                    note(&mut history, step, op, v);
                    g.begin_delete(v).unwrap();
                }
            }
            _ => {
                for _ in 0..rng.below(8) {
                    match g.reap_step() {
                        ReapStep::Idle => break,
                        ReapStep::Freed { id, .. } => note(&mut history, step, 100, id),
                        ReapStep::AbortedWait { thread } => note(&mut history, step, 101, thread),
                        ReapStep::Progress => {}
                    }
                }
            }
        }

        // Drop references the reaper has collected, so the pools stay valid.
        procs.retain(|r| g.is_live(r.id()));
        threads.retain(|r| g.is_live(r.id()));
        spaces.retain(|r| g.is_live(r.id()));
        mems.retain(|r| g.is_live(r.id()));
        eps.retain(|r| g.is_live(r.id()));
        caps.retain(|(p, _)| g.is_live(p.id()));

        if step % check_every == 0 {
            if let Err(v) = chk.check(&g) {
                std::println!("--- last ops (step, op, node) ---");
                for h in &history {
                    std::println!("  {:?}", h);
                }
                panic!("seed {} step {}: {:?}", seed, step, v);
            }
        }
    }

    // Whatever happened, tearing everything down must return every slot.
    let mut guard = 0;
    while g.node_count() > 1 {
        let victims: Vec<NodeId> =
            g.live_nodes().filter(|id| Some(*id) != g.root().map(|r| r.id())).collect();
        for v in victims {
            if g.is_live(v) {
                let _ = g.begin_delete(v);
            }
        }
        g.reap_all();
        guard += 1;
        assert!(guard < 64, "teardown did not converge");
    }
    if let Err(v) = chk.check(&g) {
        panic!("seed {} teardown: {:?}", seed, v);
    }
    assert_eq!(g.node_count(), 1, "only the root survives");
    assert_eq!(g.edge_count(), 0, "no edge outlives its endpoints");
}

#[test]
fn property_invariants_hold_under_random_operations() {
    for seed in 1..=24u64 {
        random_ops(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), 1500, 1);
    }
}

#[test]
fn property_no_leaks_under_pressure() {
    // Deliberately overrun the arenas to exercise every exhaustion path.
    for seed in 1..=6u64 {
        random_ops(seed.wrapping_mul(0xD1B5_4A32_D192_ED03), 6000, 25);
    }
}

#[test]
fn destroying_an_endpoint_aborts_its_waiters_instead_of_deadlocking() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let cpu = g.create_under_root(root, Cpu::ZERO).unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    let t = g.create_under_process(p, Thread::ZERO).unwrap();
    let ep = g.create_under_process(p, Endpoint::ZERO).unwrap();

    g.make_blocked(t, ep, WaitingAttr { role: WaitRole::Recv, badge: 7 }).unwrap();
    assert_eq!(g.body(t).unwrap().state, ThreadState::Blocked);
    check(&g);

    g.begin_delete(ep.id()).unwrap();
    let mut aborted = Vec::new();
    loop {
        match g.reap_step() {
            ReapStep::Idle => break,
            ReapStep::AbortedWait { thread } => aborted.push(thread),
            _ => {}
        }
        check(&g);
    }
    assert_eq!(aborted, [t.id()], "the reaper must name the stranded thread");
    let body = g.body(t).unwrap();
    assert_eq!(body.state, ThreadState::Inert);
    assert_eq!(body.wait_aborted, 1);

    // And the kernel can put it back on the run queue.
    g.wake(cpu, t).unwrap();
    assert_eq!(g.body(t).unwrap().state, ThreadState::Ready);
    assert_eq!(g.body(t).unwrap().wait_aborted, 0);
    check(&g);
}

#[test]
fn a_normal_wakeup_does_not_look_like_an_abort() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let cpu = g.create_under_root(root, Cpu::ZERO).unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    let t = g.create_under_process(p, Thread::ZERO).unwrap();
    let ep = g.create_under_process(p, Endpoint::ZERO).unwrap();

    g.make_blocked(t, ep, WaitingAttr { role: WaitRole::Recv, badge: 1 }).unwrap();
    assert_eq!(g.first_waiter(ep.id(), WaitRole::Recv), Some(t));
    g.wake(cpu, t).unwrap();
    assert_eq!(g.body(t).unwrap().wait_aborted, 0);
    assert_eq!(g.body(t).unwrap().state, ThreadState::Ready);
    assert!(g.first_waiter(ep.id(), WaitRole::Recv).is_none());
    check(&g);
}

// ------------------------------------------------------------- benchmarks ---
// Phase 1's go/no-go (see docs/PLAN.md). These are wall-clock timings on the
// host, not cycle counts; what matters is the shape, that every primitive is a
// handful of nanoseconds and none of them scales with graph size.

fn time_ns(iters: u64, mut f: impl FnMut()) -> f64 {
    let t0 = std::time::Instant::now();
    for _ in 0..iters {
        f();
    }
    t0.elapsed().as_nanos() as f64 / iters as f64
}

#[test]
fn bench_primitives() {
    use std::hint::black_box;

    let mut g = empty();
    let root = g.create_root().unwrap();
    let cpu = g.create_under_root(root, Cpu::ZERO).unwrap();
    let proc = g.create_under_root(root, Process::ZERO).unwrap();
    let space = g.create_under_process(proc, AddressSpace { pml4_phys: 0x5000, ..AddressSpace::ZERO }).unwrap();
    let ep = g.create_under_process(proc, Endpoint::ZERO).unwrap();

    // A populated graph, so the numbers are not measured on an empty arena.
    let threads: Vec<_> = (0..64)
        .map(|_| {
            let t = g.create_under_process(proc, Thread::ZERO).unwrap();
            g.link_in_space(t, space).unwrap();
            g.make_ready(cpu, t).unwrap();
            t
        })
        .collect();
    for _ in 0..64 {
        g.grant(proc, ep, Rights::SEND).unwrap();
    }
    let slot = g.grant(proc, ep, Rights::SEND.union(Rights::RECV)).unwrap();
    check(&g);

    let n = 2_000_000u64;
    let resolve = time_ns(n, || {
        black_box(g.resolve(black_box(proc), black_box(slot), Rights::SEND)).ok();
    });
    let header = time_ns(n, || {
        black_box(g.header(black_box(ep.id())));
    });
    let pick = time_ns(n, || {
        black_box(g.pick_next(black_box(cpu)));
    });
    let rotate = time_ns(n, || {
        black_box(g.rotate_ready(black_box(cpu)));
    });
    let link_unlink = time_ns(n / 4, || {
        let e = g.link_raw(proc.id(), EdgeKind::Holds, ep.id(), RawEdgeData::ZERO).unwrap();
        g.unlink(black_box(e)).unwrap();
    });

    let mut chk = Checker::new();
    let full_check = time_ns(2_000, || {
        chk.check(&g).unwrap();
    });

    std::println!("\n  operation                          ns/op");
    std::println!("  resolve (handle -> object)      {:>8.2}", resolve);
    std::println!("  header lookup (id -> node)      {:>8.2}", header);
    std::println!("  pick_next (run queue head)      {:>8.2}", pick);
    std::println!("  rotate_ready (round robin)      {:>8.2}", rotate);
    std::println!("  link + unlink (pair)            {:>8.2}", link_unlink);
    std::println!(
        "  full checker ({} nodes, {} edges) {:.0}",
        g.node_count(),
        g.edge_count(),
        full_check
    );

    // The go/no-go: every fast-path primitive is small and constant. These
    // bounds are loose on purpose; they exist to catch a regression into
    // something that scales with graph size, not to certify a cycle count.
    // Debug builds are an order of magnitude slower and are not measured.
    if cfg!(debug_assertions) {
        std::println!("  (debug build: timings printed, bounds not asserted)");
        black_box(&threads);
        return;
    }
    assert!(resolve < 100.0, "resolve regressed to {:.1} ns", resolve);
    assert!(pick < 100.0, "pick_next regressed to {:.1} ns", pick);
    assert!(rotate < 100.0, "rotate_ready regressed to {:.1} ns", rotate);
    assert!(link_unlink < 300.0, "link/unlink regressed to {:.1} ns", link_unlink);
    black_box(&threads);
}

/// The claim that matters more than any single number: the fast path does not
/// get slower as the graph gets bigger.
#[test]
fn fast_path_is_independent_of_graph_size() {
    use std::hint::black_box;

    fn measure(threads: usize, caps: usize) -> f64 {
        let mut g = empty();
        let root = g.create_root().unwrap();
        let cpu = g.create_under_root(root, Cpu::ZERO).unwrap();
        let proc = g.create_under_root(root, Process::ZERO).unwrap();
        let ep = g.create_under_process(proc, Endpoint::ZERO).unwrap();
        for _ in 0..threads {
            let t = g.create_under_process(proc, Thread::ZERO).unwrap();
            g.make_ready(cpu, t).unwrap();
        }
        let mut slot = 0;
        for _ in 0..caps {
            slot = g.grant(proc, ep, Rights::SEND).unwrap();
        }
        let n = 1_000_000u64;
        time_ns(n, || {
            black_box(g.resolve(black_box(proc), black_box(slot), Rights::SEND)).ok();
            black_box(g.pick_next(black_box(cpu)));
        })
    }

    let small = measure(2, 2);
    let large = measure(100, 200);
    std::println!("\n  fast path, small graph (2 threads, 2 caps):    {:.2} ns", small);
    std::println!("  fast path, large graph (100 threads, 200 caps): {:.2} ns", large);
    if cfg!(debug_assertions) {
        return;
    }
    let ratio = large / small.max(0.01);
    assert!(ratio < 3.0, "fast path scaled with graph size: {:.2}x", ratio);
}

#[test]
fn a_thread_owns_its_stack_and_takes_it_along() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    let base_nodes = g.node_count();

    let t = g.create_under_process(p, Thread::ZERO).unwrap();
    let stack = g
        .create_under_thread(t, MemoryObject { phys_base: 0x9000, pages: 4, ..MemoryObject::ZERO })
        .unwrap();
    check(&g);

    // Ownership means "dies with". Without this edge the stack outlives the
    // thread and leaks, which is what the phase 4 frame count caught.
    g.begin_delete(t.id()).unwrap();
    let mut reclaimed = Vec::new();
    loop {
        match g.reap_step() {
            ReapStep::Idle => break,
            ReapStep::Freed { reclaim: Reclaim::Frames { phys, pages, .. }, .. } => {
                reclaimed.push((phys, pages))
            }
            _ => {}
        }
    }
    assert_eq!(reclaimed, [(0x9000u64, 4u32)], "the stack must come back with the thread");
    assert!(!g.is_live(stack.id()));
    assert_eq!(g.node_count(), base_nodes);
    check(&g);
}

#[test]
fn pick_and_rotate_matches_doing_it_the_long_way() {
    let mut a = empty();
    let mut b = empty();
    let mut queues = Vec::new();
    for g in [&mut a, &mut b] {
        let root = g.create_root().unwrap();
        let cpu = g.create_under_root(root, Cpu::ZERO).unwrap();
        let p = g.create_under_root(root, Process::ZERO).unwrap();
        let ts: Vec<_> = (0..5)
            .map(|_| {
                let t = g.create_under_process(p, Thread::ZERO).unwrap();
                g.make_ready(cpu, t).unwrap();
                t
            })
            .collect();
        queues.push((cpu, ts));
    }
    let (cpu_a, ts_a) = queues[0].clone();
    let (cpu_b, _) = queues[1].clone();

    // Twelve rounds is more than two full laps of a five-deep queue.
    for round in 0..12 {
        let combined = a.pick_and_rotate(cpu_a);
        let separate = b.pick_next(cpu_b);
        b.rotate_ready(cpu_b);
        assert_eq!(
            combined.map(|r| r.id().idx()),
            separate.map(|r| r.id().idx()),
            "round {} disagreed",
            round
        );
        assert_eq!(combined.unwrap().id().idx(), ts_a[round % 5].id().idx());
        check(&a);
        check(&b);
    }
}

// ------------------------------------------------------------ range index ---

#[test]
fn the_range_index_answers_what_a_scan_would() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    let s = g.create_under_process(p, AddressSpace::ZERO).unwrap();
    let m = g.create_under_process(p, MemoryObject { pages: 64, ..MemoryObject::ZERO }).unwrap();

    // Deliberately inserted out of order: the index has to sort them.
    let starts = [0x8000u64, 0x1000, 0x5000, 0x3000, 0xB000];
    for (i, base) in starts.iter().enumerate() {
        g.link_maps(
            s,
            m,
            MapsAttr {
                vaddr: *base,
                len_pages: 1,
                off_pages: i as u32,
                prot: Prot::READ,
                flags: MapFlags::NONE,
            },
        )
        .unwrap();
        check(&g);
    }

    // Every address in a wide sweep must give the same answer as looking at
    // every mapping in turn. The index is a shortcut, not a different answer.
    for page in 0..16u64 {
        let addr = page * 0x1000 + 0x800;
        let by_scan = g
            .walk_out(s.id(), EdgeKind::Maps)
            .into_iter()
            .find(|e| MapsAttr::decode(g.edge(*e).unwrap().data).covers(addr));
        let by_index = g.find_mapping(s, addr).map(|(e, _)| e);
        assert_eq!(by_index, by_scan, "disagreement at {:#x}", addr);
    }

    // Removing from the middle keeps it sorted and keeps answering.
    let middle = g.find_mapping(s, 0x5000).unwrap().0;
    g.unlink(middle).unwrap();
    check(&g);
    assert!(g.find_mapping(s, 0x5000).is_none());
    assert!(g.find_mapping(s, 0x3000).is_some());
    assert!(g.find_mapping(s, 0x8000).is_some());
    assert_eq!(g.mapping_count(s), 4);
}

#[test]
fn overlapping_mappings_are_still_rejected_by_the_index() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    let s = g.create_under_process(p, AddressSpace::ZERO).unwrap();
    let m = g.create_under_process(p, MemoryObject { pages: 64, ..MemoryObject::ZERO }).unwrap();

    let put = |g: &mut Graph, at: u64, pages: u32| {
        g.link_maps(
            s,
            m,
            MapsAttr {
                vaddr: at,
                len_pages: pages,
                off_pages: 0,
                prot: Prot::READ,
                flags: MapFlags::NONE,
            },
        )
    };
    put(&mut g, 0x4000, 4).unwrap();
    put(&mut g, 0x1000, 1).unwrap();
    put(&mut g, 0x9000, 1).unwrap();
    // Straddling the low edge, the high edge, and entirely inside.
    assert!(matches!(put(&mut g, 0x3000, 2), Err(GraphError::RangeOverlap)));
    assert!(matches!(put(&mut g, 0x7000, 3), Err(GraphError::RangeOverlap)));
    assert!(matches!(put(&mut g, 0x5000, 1), Err(GraphError::RangeOverlap)));
    // And the gaps around them are still free.
    put(&mut g, 0x2000, 2).unwrap();
    put(&mut g, 0x8000, 1).unwrap();
    check(&g);
    assert_eq!(g.mapping_count(s), 5);
}

#[test]
fn a_full_range_index_reports_it_rather_than_overflowing() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();
    let s = g.create_under_process(p, AddressSpace::ZERO).unwrap();
    let m = g.create_under_process(p, MemoryObject { pages: 64, ..MemoryObject::ZERO }).unwrap();

    for i in 0..MAX_MAPPINGS_PER_SPACE {
        g.link_maps(
            s,
            m,
            MapsAttr {
                vaddr: 0x1000 + i as u64 * 0x2000,
                len_pages: 1,
                off_pages: 0,
                prot: Prot::READ,
                flags: MapFlags::NONE,
            },
        )
        .unwrap();
    }
    let overflow = g.link_maps(
        s,
        m,
        MapsAttr {
            vaddr: 0x9000_0000,
            len_pages: 1,
            off_pages: 0,
            prot: Prot::READ,
            flags: MapFlags::NONE,
        },
    );
    assert!(matches!(overflow, Err(GraphError::TooManyMappings)));
    check(&g);
}

#[test]
fn a_paged_object_reports_its_table_rather_than_a_range() {
    let mut g = empty();
    let root = g.create_root().unwrap();
    let p = g.create_under_root(root, Process::ZERO).unwrap();

    // A contiguous object knows where its frames are; the reaper can free them
    // from the node alone.
    g.create_under_process(p, MemoryObject { phys_base: 0x7000, pages: 3, ..MemoryObject::ZERO })
        .unwrap();
    // A paged one does not: its pages are wherever they were allocated, listed
    // in a table the graph only records the address of.
    g.create_under_process(
        p,
        MemoryObject {
            frames_phys: 0xE000,
            frames_pages: 1,
            pages: 64,
            flags: MemFlags::PAGED,
            ..MemoryObject::ZERO
        },
    )
    .unwrap();
    check(&g);

    g.begin_delete(p.id()).unwrap();
    let mut contiguous = Vec::new();
    let mut paged = Vec::new();
    loop {
        match g.reap_step() {
            ReapStep::Idle => break,
            ReapStep::Freed { reclaim: Reclaim::Frames { phys, pages, .. }, .. } => {
                contiguous.push((phys, pages))
            }
            ReapStep::Freed {
                reclaim: Reclaim::PagedMemory { table_phys, table_pages, pages }, ..
            } => paged.push((table_phys, table_pages, pages)),
            _ => {}
        }
    }
    assert_eq!(contiguous, [(0x7000u64, 3u32)]);
    assert_eq!(paged, [(0xE000u64, 1u32, 64u32)]);
    check(&g);
}

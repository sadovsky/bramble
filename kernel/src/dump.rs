//! A text rendering of the whole graph, over the serial port.
//!
//! This is the phase 2 stand-in for the `inspect` syscall of phase 8: the same
//! content, emitted by the kernel rather than requested by a process, in a
//! format `tools/graphdump.py` turns into a picture. One tool shows every
//! process, mapping, capability, wait queue and name at once, which is the
//! thing a conventional kernel cannot do (DESIGN 6.1).

use bramble_graph::body::*;
use bramble_graph::edge::*;
use bramble_graph::graph::{Graph, Ref};
use bramble_graph::id::{EdgeKind, NodeId, NodeKind};

use crate::println;
use crate::state::GRAPH;

pub const BEGIN: &str = "--- graph begin";
pub const END: &str = "--- graph end ---";

fn node_summary(g: &Graph, id: NodeId) {
    let hdr = match g.header(id) {
        Some(h) => h,
        None => return,
    };
    let flags = if hdr.is_dying() { "dying" } else { "-" };
    match id.kind() {
        Some(NodeKind::Root) => {
            let r: Ref<Root> = g.typed(id).expect("root");
            let b = g.body(r).expect("body");
            println!(
                "node {:?} {} ticks={} frames={}/{}",
                id, flags, b.ticks, b.free_frames, b.total_frames
            );
        }
        Some(NodeKind::Cpu) => {
            let r: Ref<Cpu> = g.typed(id).expect("cpu");
            let b = g.body(r).expect("body");
            println!("node {:?} {} lapic={} current={:?}", id, flags, b.lapic_id, b.current);
        }
        Some(NodeKind::MemoryObject) => {
            let r: Ref<MemoryObject> = g.typed(id).expect("memobj");
            let b = g.body(r).expect("body");
            let kind = if b.flags.contains(MemFlags::DEVICE) {
                "device"
            } else if b.flags.contains(MemFlags::PINNED) {
                "pinned"
            } else {
                "ram"
            };
            println!(
                "node {:?} {} phys={:#014x} pages={} {} maps={}",
                id, flags, b.phys_base, b.pages, kind, b.map_count
            );
        }
        Some(NodeKind::Device) => {
            let r: Ref<Device> = g.typed(id).expect("device");
            let b = g.body(r).expect("body");
            println!("node {:?} {} class={:?} io={:#06x} irq={}", id, flags, b.class, b.io_base, b.irq);
        }
        Some(NodeKind::Process) => {
            let r: Ref<Process> = g.typed(id).expect("process");
            let b = g.body(r).expect("body");
            let used = b.handles.iter().filter(|&&h| h != 0).count();
            println!("node {:?} {} handles={}", id, flags, used);
        }
        Some(NodeKind::Thread) => {
            let r: Ref<Thread> = g.typed(id).expect("thread");
            let b = g.body(r).expect("body");
            println!("node {:?} {} state={:?} cr3={:#x}", id, flags, b.state, b.cr3);
        }
        Some(NodeKind::AddressSpace) => {
            let r: Ref<AddressSpace> = g.typed(id).expect("space");
            let b = g.body(r).expect("body");
            println!("node {:?} {} pml4={:#014x} mappings={}", id, flags, b.pml4_phys, b.mapping_count);
        }
        Some(NodeKind::Endpoint) => println!("node {:?} {}", id, flags),
        None => println!("node {:?} {} <unknown kind>", id, flags),
    }
}

fn edge_line(g: &Graph, e: bramble_graph::id::EdgeId) {
    let edge = match g.edge(e) {
        Some(e) => e,
        None => return,
    };
    let kind = match edge.edge_kind() {
        Some(k) => k,
        None => return,
    };
    match kind {
        EdgeKind::Holds => {
            let a = HoldsAttr::decode(edge.data);
            println!(
                "edge Holds {:?} -> {:?} rights={:?} slot={}",
                edge.src, edge.dst, a.rights, a.slot
            );
        }
        EdgeKind::Maps => {
            let a = MapsAttr::decode(edge.data);
            println!(
                "edge Maps {:?} -> {:?} vaddr={:#014x} pages={} off={} prot={:#04x}",
                edge.src, edge.dst, a.vaddr, a.len_pages, a.off_pages, a.prot.0
            );
        }
        EdgeKind::Named => {
            let a = NamedAttr::decode(edge.data);
            println!("edge Named {:?} -> {:?} name={:?}", edge.src, edge.dst, a.as_str());
        }
        EdgeKind::Waiting => {
            let a = WaitingAttr::decode(edge.data);
            println!(
                "edge Waiting {:?} -> {:?} role={:?} badge={}",
                edge.src, edge.dst, a.role, a.badge
            );
        }
        _ => println!("edge {} {:?} -> {:?}", kind.name(), edge.src, edge.dst),
    }
}

/// Emit the whole graph. Runs under the graph lock, so it is a slow path by
/// construction; phase 8 replaces it with a snapshot the caller decodes.
pub fn dump_graph() {
    let g = GRAPH.lock();
    dump_locked(&g);
}

/// The same dump, but skipped rather than blocked if the lock is held. The
/// panic path uses this: a kernel that deadlocks while reporting a fault tells
/// you nothing at all.
pub fn dump_graph_best_effort() {
    match GRAPH.try_lock() {
        Some(g) => dump_locked(&g),
        None => println!("(graph lock held; no dump)"),
    }
}

fn dump_locked(g: &Graph) {
    println!("{} seq={} nodes={} edges={} ---", BEGIN, g.seq(), g.node_count(), g.edge_count());
    for id in g.live_nodes() {
        node_summary(g, id);
    }
    for e in g.live_edges() {
        edge_line(g, e);
    }
    println!("{}", END);
}

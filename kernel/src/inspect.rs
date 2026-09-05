//! Serialising the whole graph for userspace.
//!
//! Phase 8 replaces this with the sorted-edge-table format of DESIGN 5.4. The
//! shape is already that: a header carrying the non-graph register, then nodes,
//! then edges. What phase 8 adds is ordering guarantees and an offset table, so
//! a decoder written now keeps working.

use bramble_abi::*;
use bramble_graph::body::*;
use bramble_graph::edge::{EdgeAttr, HoldsAttr, MapsAttr, NamedAttr, WaitingAttr};
use bramble_graph::graph::{Graph, Ref};
use bramble_graph::id::{EdgeKind, NodeKind};

fn write_at<T: Copy>(buf: &mut [u8], offset: usize, value: T) {
    let size = core::mem::size_of::<T>();
    if offset + size > buf.len() {
        return;
    }
    // SAFETY: bounds checked above; `T` is a plain `repr(C)` record with no
    // padding requirements beyond what the buffer already satisfies.
    unsafe {
        core::ptr::copy_nonoverlapping(
            &value as *const T as *const u8,
            buf.as_mut_ptr().add(offset),
            size,
        );
    }
}

fn node_record(g: &Graph, id: bramble_graph::id::NodeId) -> NodeRecord {
    let hdr = g.header(id).expect("live node");
    let mut r = NodeRecord {
        id: id.0,
        kind: id.kind_raw(),
        flags: hdr.flags,
        ..NodeRecord::default()
    };
    match id.kind() {
        Some(NodeKind::Root) => {
            if let Some(n) = g.typed::<Root>(id).and_then(|x| g.body(x)) {
                r.a = n.ticks;
                r.b = n.free_frames;
            }
        }
        Some(NodeKind::Cpu) => {
            if let Some(n) = g.typed::<Cpu>(id).and_then(|x| g.body(x)) {
                r.a = n.current.0;
            }
        }
        Some(NodeKind::Process) => {
            if let Some(n) = g.typed::<Process>(id).and_then(|x| g.body(x)) {
                r.a = n.handles.iter().filter(|&&h| h != 0).count() as u64;
                r.b = n.exit_code as i64 as u64;
            }
        }
        Some(NodeKind::Thread) => {
            if let Some(n) = g.typed::<Thread>(id).and_then(|x| g.body(x)) {
                r.a = n.state as u64;
                r.b = n.cr3;
            }
        }
        Some(NodeKind::AddressSpace) => {
            if let Some(n) = g.typed::<AddressSpace>(id).and_then(|x| g.body(x)) {
                r.a = n.pml4_phys;
                r.b = n.mapping_count as u64;
            }
        }
        Some(NodeKind::MemoryObject) => {
            if let Some(n) = g.typed::<MemoryObject>(id).and_then(|x| g.body(x)) {
                r.a = n.phys_base;
                r.b = n.pages as u64 | ((n.flags.0 as u64) << 32);
            }
        }
        Some(NodeKind::Device) => {
            if let Some(n) = g.typed::<Device>(id).and_then(|x| g.body(x)) {
                r.a = n.io_base as u64;
                r.b = n.class as u64;
            }
        }
        _ => {}
    }
    r
}

fn edge_record(g: &Graph, eid: bramble_graph::id::EdgeId) -> EdgeRecord {
    let e = g.edge(eid).expect("live edge");
    let mut r =
        EdgeRecord { src: e.src.0, dst: e.dst.0, kind: e.kind, ..EdgeRecord::default() };
    match e.edge_kind() {
        Some(EdgeKind::Holds) => {
            let a = HoldsAttr::decode(e.data);
            r.a = a.rights.0 as u64;
            r.b = a.slot as u64;
        }
        Some(EdgeKind::Maps) => {
            let a = MapsAttr::decode(e.data);
            r.a = a.vaddr;
            r.b = a.len_pages as u64 | ((a.prot.0 as u64) << 32);
        }
        Some(EdgeKind::Named) => {
            // The first sixteen bytes of the name, so a decoder can label the
            // picture. Longer names need the string table phase 8 brings.
            let n = NamedAttr::decode(e.data);
            let mut buf = [0u8; 16];
            let len = (n.len as usize).min(16);
            buf[..len].copy_from_slice(&n.bytes[..len]);
            r.a = u64::from_le_bytes(buf[..8].try_into().unwrap());
            r.b = u64::from_le_bytes(buf[8..].try_into().unwrap());
        }
        Some(EdgeKind::Waiting) => {
            let a = WaitingAttr::decode(e.data);
            r.a = a.role as u64;
            r.b = a.badge;
        }
        _ => {}
    }
    r
}

/// How many virtual edges this snapshot will carry (DESIGN 5.4).
pub fn virtual_edge_count(g: &Graph) -> u32 {
    let mut n = 0;
    for id in g.live_nodes() {
        if id.kind() == Some(NodeKind::Cpu) {
            if let Some(c) = g.typed::<Cpu>(id).and_then(|c| g.body(c)) {
                if !c.current.is_null() {
                    n += 1;
                }
            }
        }
    }
    n
}

/// Bytes a snapshot of the current graph needs.
pub fn snapshot_size(g: &Graph) -> usize {
    inspect_size(g.node_count(), g.edge_count() + virtual_edge_count(g))
}

/// Write a snapshot into `buf`, returning the bytes used.
///
/// Edges are emitted grouped by source node and then by kind, and an adjacency
/// table says where each node's group starts. That is DESIGN 5.4's compressed
/// sparse row: a reader gets a usable index for free and never has to search.
/// Within a group the order is list order, not sorted by target, because for
/// `Ready` and `Waiting` that order *is* the queue and losing it would throw
/// away the most interesting thing in the snapshot.
pub fn write_snapshot(g: &Graph, buf: &mut [u8]) -> usize {
    let nodes = g.node_count();
    let virtuals = virtual_edge_count(g);
    let edges = g.edge_count() + virtuals;
    let node_offset = core::mem::size_of::<InspectHeader>();
    let adjacency_offset = node_offset + nodes as usize * core::mem::size_of::<NodeRecord>();
    let edge_offset =
        adjacency_offset + nodes as usize * core::mem::size_of::<AdjacencyEntry>();
    let total = edge_offset + edges as usize * core::mem::size_of::<EdgeRecord>();
    if buf.len() < total {
        return 0;
    }

    let root = g.root().and_then(|r| g.body(r).copied());
    write_at(
        buf,
        0,
        InspectHeader {
            magic: INSPECT_MAGIC,
            version: INSPECT_VERSION,
            seq: g.seq(),
            node_count: nodes,
            edge_count: edges,
            node_offset: node_offset as u32,
            adjacency_offset: adjacency_offset as u32,
            edge_offset: edge_offset as u32,
            _reserved: 0,
            total_frames: root.map(|r| r.total_frames).unwrap_or(0),
            free_frames: root.map(|r| r.free_frames).unwrap_or(0),
            ticks: root.map(|r| r.ticks).unwrap_or(0),
        },
    );

    let mut node_at = node_offset;
    let mut adj_at = adjacency_offset;
    let mut edge_at = edge_offset;
    let mut emitted = 0u32;

    for id in g.live_nodes() {
        write_at(buf, node_at, node_record(g, id));
        node_at += core::mem::size_of::<NodeRecord>();

        let first = emitted;
        for k in 0..bramble_graph::id::N_EDGE_KINDS {
            let kind = EdgeKind::from_u8(k as u8).expect("kind in range");
            for eid in g.out_edges(id, kind) {
                write_at(buf, edge_at, edge_record(g, eid));
                edge_at += core::mem::size_of::<EdgeRecord>();
                emitted += 1;
            }
        }
        // The one relationship the kernel keeps as a field rather than an edge,
        // reported as an edge and flagged as such.
        if id.kind() == Some(NodeKind::Cpu) {
            if let Some(c) = g.typed::<Cpu>(id).and_then(|c| g.body(c)) {
                if !c.current.is_null() {
                    write_at(
                        buf,
                        edge_at,
                        EdgeRecord {
                            src: id.0,
                            dst: c.current.0,
                            kind: EDGE_KIND_RUNNING,
                            flags: EDGE_FLAG_VIRTUAL,
                            ..EdgeRecord::default()
                        },
                    );
                    edge_at += core::mem::size_of::<EdgeRecord>();
                    emitted += 1;
                }
            }
        }
        write_at(buf, adj_at, AdjacencyEntry { first, count: emitted - first });
        adj_at += core::mem::size_of::<AdjacencyEntry>();
    }

    let _: Option<Ref<Root>> = None;
    total
}

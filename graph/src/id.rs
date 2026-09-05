//! Identifiers, node kinds, and edge kinds.
//!
//! Both id types are 8-byte `Copy` scalars. A generation of zero is never
//! handed out, so all-zero memory is a valid *null* id, which is what lets the
//! whole graph live in `.bss` with no initialisation loop (DESIGN 3.2).

use core::fmt;

/// Node kinds. `Root` is 0 so that a zeroed id is `NodeId::NULL` (gen 0).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum NodeKind {
    Root = 0,
    Cpu = 1,
    Process = 2,
    Thread = 3,
    AddressSpace = 4,
    MemoryObject = 5,
    Endpoint = 6,
    Device = 7,
}

pub const N_NODE_KINDS: usize = 8;

impl NodeKind {
    pub const fn from_u8(v: u8) -> Option<NodeKind> {
        match v {
            0 => Some(NodeKind::Root),
            1 => Some(NodeKind::Cpu),
            2 => Some(NodeKind::Process),
            3 => Some(NodeKind::Thread),
            4 => Some(NodeKind::AddressSpace),
            5 => Some(NodeKind::MemoryObject),
            6 => Some(NodeKind::Endpoint),
            7 => Some(NodeKind::Device),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            NodeKind::Root => "Root",
            NodeKind::Cpu => "Cpu",
            NodeKind::Process => "Process",
            NodeKind::Thread => "Thread",
            NodeKind::AddressSpace => "AddressSpace",
            NodeKind::MemoryObject => "MemoryObject",
            NodeKind::Endpoint => "Endpoint",
            NodeKind::Device => "Device",
        }
    }
}

/// Edge kinds (DESIGN 4.2). Seven for v1.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum EdgeKind {
    Owns = 0,
    Holds = 1,
    InSpace = 2,
    Maps = 3,
    Ready = 4,
    Waiting = 5,
    Named = 6,
}

pub const N_EDGE_KINDS: usize = 7;

impl EdgeKind {
    pub const fn from_u8(v: u8) -> Option<EdgeKind> {
        match v {
            0 => Some(EdgeKind::Owns),
            1 => Some(EdgeKind::Holds),
            2 => Some(EdgeKind::InSpace),
            3 => Some(EdgeKind::Maps),
            4 => Some(EdgeKind::Ready),
            5 => Some(EdgeKind::Waiting),
            6 => Some(EdgeKind::Named),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            EdgeKind::Owns => "Owns",
            EdgeKind::Holds => "Holds",
            EdgeKind::InSpace => "InSpace",
            EdgeKind::Maps => "Maps",
            EdgeKind::Ready => "Ready",
            EdgeKind::Waiting => "Waiting",
            EdgeKind::Named => "Named",
        }
    }
}

/// The runtime endpoint-compatibility table (invariant I3).
///
/// Typed call sites enforce this at compile time via the `link_*` methods; this
/// function is the runtime check on the generic path and in the checker.
pub const fn compatible(kind: EdgeKind, src: NodeKind, dst: NodeKind) -> bool {
    use EdgeKind::*;
    use NodeKind::*;
    match (kind, src, dst) {
        // Storage and lifetime. Root owns anything; a process owns the objects
        // it creates, including child processes.
        (Owns, Root, _) => true,
        (
            Owns,
            Process,
            Process | Thread | AddressSpace | MemoryObject | Endpoint,
        ) => true,
        // A thread owns its kernel stack. Ownership means "dies with", and a
        // kernel stack dies with its thread; without this the stack outlives
        // the thread and leaks, which is exactly what phase 4 measured.
        (Owns, Thread, MemoryObject) => true,
        (Owns, _, _) => false,

        // Capabilities: only processes hold them.
        (
            Holds,
            Process,
            Root | Process | Thread | AddressSpace | MemoryObject | Endpoint | Device,
        ) => true,
        (Holds, _, _) => false,

        (InSpace, Thread, AddressSpace) => true,
        (InSpace, _, _) => false,

        (Maps, AddressSpace, MemoryObject) => true,
        (Maps, _, _) => false,

        (Ready, Cpu, Thread) => true,
        (Ready, _, _) => false,

        (Waiting, Thread, Endpoint | Device) => true,
        (Waiting, _, _) => false,

        (Named, Root, _) => true,
        (Named, _, _) => false,
    }
}

/// `kind:8 | idx:24 | gen:32`. Encoding the kind in the id sends a lookup
/// straight to the right slab with no dispatch table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct NodeId(pub u64);

impl NodeId {
    pub const NULL: NodeId = NodeId(0);
    pub const MAX_IDX: u32 = 0x00FF_FFFF;

    pub const fn new(kind: NodeKind, idx: u32, generation: u32) -> NodeId {
        NodeId((kind as u64) | (((idx as u64) & 0x00FF_FFFF) << 8) | ((generation as u64) << 32))
    }
    pub const fn kind_raw(self) -> u8 {
        self.0 as u8
    }
    pub const fn idx(self) -> u32 {
        ((self.0 >> 8) & 0x00FF_FFFF) as u32
    }
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
    /// A null id is one that was never handed out: generation zero.
    pub const fn is_null(self) -> bool {
        self.generation() == 0
    }
    pub fn kind(self) -> Option<NodeKind> {
        NodeKind::from_u8(self.kind_raw())
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_null() {
            return f.write_str("NodeId(null)");
        }
        match self.kind() {
            Some(k) => write!(f, "{}#{}.{}", k.name(), self.idx(), self.generation()),
            None => write!(f, "Node?{}#{}.{}", self.kind_raw(), self.idx(), self.generation()),
        }
    }
}

/// `idx:32 | gen:32`. Edge slot 0 is a permanent sentinel meaning "no edge", so
/// that a zeroed list head is an empty list.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct EdgeId(pub u64);

impl EdgeId {
    pub const NULL: EdgeId = EdgeId(0);

    pub const fn new(idx: u32, generation: u32) -> EdgeId {
        EdgeId((idx as u64) | ((generation as u64) << 32))
    }
    pub const fn idx(self) -> u32 {
        self.0 as u32
    }
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
    pub const fn is_null(self) -> bool {
        self.generation() == 0
    }
}

impl fmt::Debug for EdgeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_null() {
            f.write_str("EdgeId(null)")
        } else {
            write!(f, "Edge#{}.{}", self.idx(), self.generation())
        }
    }
}

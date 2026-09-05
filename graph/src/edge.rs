//! Edges: 64 bytes, cross-linked into two circular doubly-linked lists at once
//! (the source's out-list for its kind, the target's in-list for its kind).
//!
//! Circular rather than null-terminated so that a single head pointer gives
//! O(1) push-tail, pop-head and unlink. That is what makes `Ready` and
//! `Waiting` real FIFO queues without a tail pointer per list (DESIGN 3.2).

use crate::id::{EdgeKind, NodeId};
use crate::limits::MAX_NAME_LEN;

/// Type-erased edge attributes. 24 bytes, little-endian, decoded by the
/// per-kind codecs below.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct RawEdgeData(pub [u8; 24]);

impl RawEdgeData {
    pub const ZERO: RawEdgeData = RawEdgeData([0; 24]);

    #[inline]
    fn put_u64(&mut self, at: usize, v: u64) {
        self.0[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_u32(&mut self, at: usize, v: u32) {
        self.0[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn get_u64(&self, at: usize) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.0[at..at + 8]);
        u64::from_le_bytes(b)
    }
    #[inline]
    fn get_u32(&self, at: usize) -> u32 {
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.0[at..at + 4]);
        u32::from_le_bytes(b)
    }
}

/// Attribute payload for one edge kind.
pub trait EdgeAttr: Copy {
    const KIND: EdgeKind;
    fn encode(self) -> RawEdgeData;
    fn decode(raw: RawEdgeData) -> Self;
}

// --------------------------------------------------------------- payloads ---

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct OwnsAttr;

impl EdgeAttr for OwnsAttr {
    const KIND: EdgeKind = EdgeKind::Owns;
    fn encode(self) -> RawEdgeData {
        RawEdgeData::ZERO
    }
    fn decode(_: RawEdgeData) -> Self {
        OwnsAttr
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldsAttr {
    pub rights: crate::body::Rights,
    /// The userspace handle number this capability answers to.
    pub slot: u32,
}

impl EdgeAttr for HoldsAttr {
    const KIND: EdgeKind = EdgeKind::Holds;
    fn encode(self) -> RawEdgeData {
        let mut r = RawEdgeData::ZERO;
        r.put_u32(0, self.rights.0);
        r.put_u32(4, self.slot);
        r
    }
    fn decode(raw: RawEdgeData) -> Self {
        HoldsAttr { rights: crate::body::Rights(raw.get_u32(0)), slot: raw.get_u32(4) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct InSpaceAttr;

impl EdgeAttr for InSpaceAttr {
    const KIND: EdgeKind = EdgeKind::InSpace;
    fn encode(self) -> RawEdgeData {
        RawEdgeData::ZERO
    }
    fn decode(_: RawEdgeData) -> Self {
        InSpaceAttr
    }
}

/// Page protection bits carried by a `Maps` edge. The page tables must satisfy
/// these exactly (invariant I5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct Prot(pub u8);

impl Prot {
    pub const NONE: Prot = Prot(0);
    pub const READ: Prot = Prot(1 << 0);
    pub const WRITE: Prot = Prot(1 << 1);
    pub const EXEC: Prot = Prot(1 << 2);
    pub const USER: Prot = Prot(1 << 3);
    pub const fn contains(self, o: Prot) -> bool {
        self.0 & o.0 == o.0
    }
    pub const fn union(self, o: Prot) -> Prot {
        Prot(self.0 | o.0)
    }
    pub const RW: Prot = Prot(Prot::READ.0 | Prot::WRITE.0);
    pub const RWU: Prot = Prot(Prot::READ.0 | Prot::WRITE.0 | Prot::USER.0);
    pub const RXU: Prot = Prot(Prot::READ.0 | Prot::EXEC.0 | Prot::USER.0);
}

/// How a mapping is realised in the page tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct MapFlags(pub u8);

impl MapFlags {
    pub const NONE: MapFlags = MapFlags(0);
    /// The edge exists but the page-table entries do not. They are written one
    /// page at a time, when the page is first touched. A mapping is still a
    /// mapping: the graph says the memory is there, and the hardware is brought
    /// up to date lazily rather than eagerly.
    pub const LAZY: MapFlags = MapFlags(1 << 0);
    pub const fn contains(self, o: MapFlags) -> bool {
        self.0 & o.0 == o.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapsAttr {
    pub vaddr: u64,
    pub len_pages: u32,
    /// Offset into the memory object, in pages.
    pub off_pages: u32,
    pub prot: Prot,
    pub flags: MapFlags,
}

impl MapsAttr {
    /// Does this mapping cover `vaddr`?
    pub const fn covers(&self, vaddr: u64) -> bool {
        vaddr >= self.vaddr && vaddr < self.end()
    }

    /// Exclusive end of the virtual range, in bytes.
    pub const fn end(&self) -> u64 {
        self.vaddr + (self.len_pages as u64) * 4096
    }
    pub const fn overlaps(&self, other: &MapsAttr) -> bool {
        self.vaddr < other.end() && other.vaddr < self.end()
    }
}

impl EdgeAttr for MapsAttr {
    const KIND: EdgeKind = EdgeKind::Maps;
    fn encode(self) -> RawEdgeData {
        let mut r = RawEdgeData::ZERO;
        r.put_u64(0, self.vaddr);
        r.put_u32(8, self.len_pages);
        r.put_u32(12, self.off_pages);
        r.0[16] = self.prot.0;
        r.0[17] = self.flags.0;
        r
    }
    fn decode(raw: RawEdgeData) -> Self {
        MapsAttr {
            vaddr: raw.get_u64(0),
            len_pages: raw.get_u32(8),
            off_pages: raw.get_u32(12),
            prot: Prot(raw.0[16]),
            flags: MapFlags(raw.0[17]),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ReadyAttr {
    /// Unused by v1's round robin; present so priorities need no format change.
    pub prio: u8,
}

impl EdgeAttr for ReadyAttr {
    const KIND: EdgeKind = EdgeKind::Ready;
    fn encode(self) -> RawEdgeData {
        let mut r = RawEdgeData::ZERO;
        r.0[0] = self.prio;
        r
    }
    fn decode(raw: RawEdgeData) -> Self {
        ReadyAttr { prio: raw.0[0] }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WaitRole {
    Send = 0,
    Recv = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitingAttr {
    pub role: WaitRole,
    pub badge: u64,
}

impl EdgeAttr for WaitingAttr {
    const KIND: EdgeKind = EdgeKind::Waiting;
    fn encode(self) -> RawEdgeData {
        let mut r = RawEdgeData::ZERO;
        r.0[0] = self.role as u8;
        r.put_u64(8, self.badge);
        r
    }
    fn decode(raw: RawEdgeData) -> Self {
        WaitingAttr {
            role: if raw.0[0] == 0 { WaitRole::Send } else { WaitRole::Recv },
            badge: raw.get_u64(8),
        }
    }
}

/// An inline name. Longer names need a post-v1 string table (DESIGN 4.4).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NamedAttr {
    pub bytes: [u8; MAX_NAME_LEN],
    pub len: u8,
}

impl NamedAttr {
    /// Returns `None` if the name does not fit inline.
    pub fn new(name: &str) -> Option<NamedAttr> {
        let s = name.as_bytes();
        if s.len() > MAX_NAME_LEN {
            return None;
        }
        let mut bytes = [0u8; MAX_NAME_LEN];
        bytes[..s.len()].copy_from_slice(s);
        Some(NamedAttr { bytes, len: s.len() as u8 })
    }
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("<invalid>")
    }
    pub fn matches(&self, name: &str) -> bool {
        self.as_str() == name
    }
}

impl core::fmt::Debug for NamedAttr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl EdgeAttr for NamedAttr {
    const KIND: EdgeKind = EdgeKind::Named;
    fn encode(self) -> RawEdgeData {
        let mut r = RawEdgeData::ZERO;
        r.0[..MAX_NAME_LEN].copy_from_slice(&self.bytes);
        r.0[MAX_NAME_LEN] = self.len;
        r
    }
    fn decode(raw: RawEdgeData) -> Self {
        let mut bytes = [0u8; MAX_NAME_LEN];
        bytes.copy_from_slice(&raw.0[..MAX_NAME_LEN]);
        let len = core::cmp::min(raw.0[MAX_NAME_LEN], MAX_NAME_LEN as u8);
        NamedAttr { bytes, len }
    }
}

// ------------------------------------------------------------------ edge ---

/// Which of an edge's two list linkages to operate on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// The source node's out-list.
    Out,
    /// The target node's in-list.
    In,
}

pub const EDGE_FLAG_NONE: u8 = 0;

/// One relationship. Exactly one cache line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub out_next: u32,
    pub out_prev: u32,
    pub in_next: u32,
    pub in_prev: u32,
    /// Even means free, odd means live. Zero means never used.
    pub generation: u32,
    pub kind: u8,
    pub flags: u8,
    pub _pad: u16,
    pub data: RawEdgeData,
}

impl Edge {
    pub const ZERO: Edge = Edge {
        src: NodeId::NULL,
        dst: NodeId::NULL,
        out_next: 0,
        out_prev: 0,
        in_next: 0,
        in_prev: 0,
        generation: 0,
        kind: 0,
        flags: 0,
        _pad: 0,
        data: RawEdgeData::ZERO,
    };

    #[inline]
    pub fn edge_kind(&self) -> Option<EdgeKind> {
        EdgeKind::from_u8(self.kind)
    }
    #[inline]
    pub fn is_live(&self) -> bool {
        self.generation & 1 == 1
    }
    #[inline]
    pub fn next(&self, dir: Dir) -> u32 {
        match dir {
            Dir::Out => self.out_next,
            Dir::In => self.in_next,
        }
    }
    #[inline]
    pub fn prev(&self, dir: Dir) -> u32 {
        match dir {
            Dir::Out => self.out_prev,
            Dir::In => self.in_prev,
        }
    }
    #[inline]
    pub fn set_next(&mut self, dir: Dir, v: u32) {
        match dir {
            Dir::Out => self.out_next = v,
            Dir::In => self.in_next = v,
        }
    }
    #[inline]
    pub fn set_prev(&mut self, dir: Dir, v: u32) {
        match dir {
            Dir::Out => self.out_prev = v,
            Dir::In => self.in_prev = v,
        }
    }
}

const _: () = assert!(core::mem::size_of::<Edge>() == 64);

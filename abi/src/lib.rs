//! The contract between the kernel and userspace.
//!
//! Deliberately tiny and dependency-free: userspace links this and nothing
//! else. Every argument that names a kernel object is a *slot number* in the
//! calling process's handle table, never a `NodeId`. Userspace has no way to
//! name an object it has not been given.

#![no_std]

// ------------------------------------------------------------- syscalls ---
//
// Calling convention:
//
//   rax                  call number
//   rdi, rsi, rdx, r10   arguments
//   rax                  result, or a negative error code
//
// `r10` rather than `rcx` because the `syscall` instruction overwrites `rcx`
// with the return address before the kernel sees it.
//
// **Only `rcx` and `r11` are clobbered.** Every other register is preserved
// across the call, including the caller-saved ones. The kernel's entry stub
// saves and restores them, because a caller's compiler has no reason to expect
// otherwise and will happily keep a live pointer in `rsi` across a `syscall`.

pub const SYS_EXIT: u64 = 0;
pub const SYS_YIELD: u64 = 1;
pub const SYS_WRITE: u64 = 2;
pub const SYS_LOOKUP: u64 = 3;
pub const SYS_INSPECT: u64 = 4;
pub const SYS_RIGHTS: u64 = 5;
/// send(endpoint_slot, words_ptr, capability_slot_or_zero) -> 0
pub const SYS_SEND: u64 = 6;
/// recv(endpoint_slot, words_ptr) -> slot the received capability landed in, or 0
pub const SYS_RECV: u64 = 7;
/// spawn(image_slot) -> a slot holding the new process, created but not started
pub const SYS_SPAWN: u64 = 8;
/// grant(process_slot, capability_slot, rights_mask) -> the slot it landed in
pub const SYS_GRANT: u64 = 9;
/// start(process_slot) -> 0
pub const SYS_START: u64 = 10;
/// kill(process_slot) -> 0
pub const SYS_KILL: u64 = 11;
/// endpoint_create() -> a slot holding a new endpoint the caller owns
pub const SYS_ENDPOINT: u64 = 12;
/// check() -> 0 if every kernel invariant holds, or a negative code
pub const SYS_CHECK: u64 = 13;
/// mem_create(pages, paged) -> a slot holding zeroed memory the caller owns.
/// `paged` asks for memory whose pages come into existence as they are touched,
/// rather than being allocated up front.
pub const SYS_MEM_CREATE: u64 = 14;
/// map(memory_slot, vaddr, prot, lazy) -> 0
pub const SYS_MAP: u64 = 15;
/// unmap(vaddr) -> 0
pub const SYS_UNMAP: u64 = 16;

/// Page protection bits for `map`. These match the kernel's `Prot`.
pub const P_READ: u64 = 1 << 0;
pub const P_WRITE: u64 = 1 << 1;
pub const P_EXEC: u64 = 1 << 2;

/// Words in one IPC message. Sized to be copied without ceremony; anything
/// larger is what shared memory and a capability to it are for.
pub const MSG_WORDS: usize = 8;

// --------------------------------------------------------------- errors ---
// Returned as a negative value in the syscall's return register.

pub const E_BADCALL: i64 = -1;
/// The slot is empty, or holds a capability to something that is being
/// destroyed.
pub const E_BADHANDLE: i64 = -2;
/// The capability exists but does not carry the right this call needs.
pub const E_PERM: i64 = -3;
/// A pointer argument is not backed by a mapping with the required access.
pub const E_FAULT: i64 = -4;
/// The buffer is too small; the return value says how much was needed.
pub const E_TOOSMALL: i64 = -5;
pub const E_NOTFOUND: i64 = -6;
pub const E_NOSPACE: i64 = -7;
/// The object is not of the kind this call works on.
pub const E_BADKIND: i64 = -8;
/// The image could not be loaded.
pub const E_BADIMAGE: i64 = -9;

pub fn error_name(code: i64) -> &'static str {
    match code {
        E_BADCALL => "bad syscall number",
        E_BADHANDLE => "bad handle",
        E_PERM => "missing right",
        E_FAULT => "bad pointer",
        E_TOOSMALL => "buffer too small",
        E_NOTFOUND => "not found",
        E_NOSPACE => "no space",
        E_BADKIND => "wrong kind of object",
        E_BADIMAGE => "not a loadable image",
        _ => "unknown error",
    }
}

// --------------------------------------------------------------- rights ---
// These must match `bramble_graph::body::Rights`, and a test in the kernel
// asserts that they do.

pub const R_READ: u32 = 1 << 0;
pub const R_WRITE: u32 = 1 << 1;
pub const R_MAP: u32 = 1 << 2;
pub const R_SEND: u32 = 1 << 3;
pub const R_RECV: u32 = 1 << 4;
pub const R_GRANT: u32 = 1 << 5;
pub const R_LOOKUP: u32 = 1 << 6;
pub const R_MANAGE: u32 = 1 << 7;

// -------------------------------------------------------------- inspect ---
// A snapshot of the whole graph, written into a caller-supplied buffer. Phase 8
// replaces this with the sorted-edge-table format of DESIGN 5.4; the shape is
// already the same, so decoders written now keep working.

pub const INSPECT_MAGIC: u32 = 0x4272_616D; // "Bram"
pub const INSPECT_VERSION: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InspectHeader {
    pub magic: u32,
    pub version: u32,
    /// The graph's mutation counter, so two snapshots can be told apart.
    pub seq: u64,
    pub node_count: u32,
    pub edge_count: u32,
    /// Byte offsets from the start of the buffer.
    pub node_offset: u32,
    pub edge_offset: u32,
    /// One `AdjacencyEntry` per node, in node-table order: where that node's
    /// out-edges start in the edge table and how many there are. Edges are
    /// emitted grouped by source and then by kind, so this is a compressed
    /// sparse row and a reader never has to search.
    pub adjacency_offset: u32,
    pub _reserved: u32,
    /// The non-graph register, carried alongside so the snapshot really is the
    /// whole of the kernel's state (DESIGN 4.4).
    pub total_frames: u64,
    pub free_frames: u64,
    pub ticks: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NodeRecord {
    pub id: u64,
    pub kind: u8,
    pub flags: u8,
    pub _pad: [u8; 6],
    /// Kind-specific: physical base, page-table root, io port, and so on.
    pub a: u64,
    pub b: u64,
}

/// Where one node's out-edges live in the edge table.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AdjacencyEntry {
    pub first: u32,
    pub count: u32,
}

/// An edge that is not stored as an edge.
///
/// `Running` is a field on the cpu, not a `Ready` edge, because putting it in
/// the graph would cost two list splices on every context switch (DESIGN 5.3).
/// It is emitted here anyway, flagged, so that a snapshot really does show the
/// whole of the kernel's state rather than the whole of its edge set.
pub const EDGE_FLAG_VIRTUAL: u8 = 1 << 0;
/// The kind number given to virtual `Running` edges. It is one past the real
/// kinds, so a decoder that does not know about it will not mistake it for one.
pub const EDGE_KIND_RUNNING: u8 = 7;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct EdgeRecord {
    pub src: u64,
    pub dst: u64,
    pub kind: u8,
    pub flags: u8,
    pub _pad: [u8; 6],
    /// Kind-specific: rights and slot, virtual address, wait role.
    pub a: u64,
    pub b: u64,
}

pub const NODE_KIND_NAMES: [&str; 8] =
    ["Root", "Cpu", "Process", "Thread", "AddressSpace", "MemoryObject", "Endpoint", "Device"];
pub const EDGE_KIND_NAMES: [&str; 8] =
    ["Owns", "Holds", "InSpace", "Maps", "Ready", "Waiting", "Named", "Running"];

/// Total bytes an inspect snapshot of this size occupies. `edges` must include
/// the virtual ones.
pub const fn inspect_size(nodes: u32, edges: u32) -> usize {
    core::mem::size_of::<InspectHeader>()
        + nodes as usize * core::mem::size_of::<NodeRecord>()
        + nodes as usize * core::mem::size_of::<AdjacencyEntry>()
        + edges as usize * core::mem::size_of::<EdgeRecord>()
}

//! Static arena sizes (DESIGN appendix B).
//!
//! v1 needs fewer than fifty nodes; these are sized 20x to 50x larger so that
//! exhaustion paths can actually be tested. They are `const` rather than
//! generic parameters on purpose: the whole point is one statically sized
//! graph living in `.bss`. Post-v1 growth is by chunking (DESIGN 3.6), which
//! changes `Slab` internals and no callers.

pub const MAX_ROOTS: usize = 1;
pub const MAX_CPUS: usize = 8;
pub const MAX_PROCESSES: usize = 64;
pub const MAX_THREADS: usize = 128;
pub const MAX_SPACES: usize = 64;
pub const MAX_MEMOBJS: usize = 512;
pub const MAX_ENDPOINTS: usize = 128;
pub const MAX_DEVICES: usize = 16;

/// Edge slot 0 is a reserved sentinel, so usable edges are `MAX_EDGES - 1`.
pub const MAX_EDGES: usize = 4096;

/// Capability slots per process (the handle table, DESIGN 4.4).
pub const HANDLE_SLOTS: usize = 256;

/// Longest name storable inline in a `Named` edge.
pub const MAX_NAME_LEN: usize = 23;

/// Mappings one address space may hold.
///
/// This is a hard cap, and it exists because the range index lives inside the
/// `AddressSpace` body rather than in allocated memory. Post-v1 growth would
/// make it a chunked side table like the arenas themselves.
pub const MAX_MAPPINGS_PER_SPACE: usize = 32;

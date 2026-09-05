//! Node bodies: the fixed-size payload of each node kind (DESIGN 4.1).
//!
//! Bodies are plain data. The graph crate deliberately knows nothing about x86:
//! `cr3` and `pml4_phys` are just integers here, and the kernel gives them
//! meaning.

use crate::id::NodeKind;
use crate::limits::{HANDLE_SLOTS, MAX_MAPPINGS_PER_SPACE};

/// Implemented by every node body. `ZERO` is what makes `.bss` a valid graph.
pub trait NodeBody: Copy {
    const KIND: NodeKind;
    const ZERO: Self;
}

// ---------------------------------------------------------------- rights ---

/// Capability rights carried by a `Holds` edge (DESIGN appendix A).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct Rights(pub u32);

impl Rights {
    pub const NONE: Rights = Rights(0);
    pub const READ: Rights = Rights(1 << 0);
    pub const WRITE: Rights = Rights(1 << 1);
    pub const MAP: Rights = Rights(1 << 2);
    pub const SEND: Rights = Rights(1 << 3);
    pub const RECV: Rights = Rights(1 << 4);
    pub const GRANT: Rights = Rights(1 << 5);
    pub const LOOKUP: Rights = Rights(1 << 6);
    pub const MANAGE: Rights = Rights(1 << 7);
    pub const ALL: Rights = Rights(0xFF);

    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }
    pub const fn intersect(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl core::fmt::Debug for Rights {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const NAMES: [(u32, &str); 8] = [
            (1 << 0, "R"),
            (1 << 1, "W"),
            (1 << 2, "M"),
            (1 << 3, "s"),
            (1 << 4, "r"),
            (1 << 5, "g"),
            (1 << 6, "l"),
            (1 << 7, "A"),
        ];
        if self.0 == 0 {
            return f.write_str("-");
        }
        for (bit, name) in NAMES {
            if self.0 & bit != 0 {
                f.write_str(name)?;
            }
        }
        Ok(())
    }
}

// ----------------------------------------------------------------- bodies ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Root {
    pub ticks: u64,
    pub total_frames: u64,
    pub free_frames: u64,
    pub boot_flags: u64,
}

impl NodeBody for Root {
    const KIND: NodeKind = NodeKind::Root;
    const ZERO: Self = Root { ticks: 0, total_frames: 0, free_frames: 0, boot_flags: 0 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Cpu {
    /// The running thread. A field rather than an edge: two list splices per
    /// context switch for no query benefit (DESIGN 5.3 compromise 2).
    pub current: crate::id::NodeId,
    pub idle_thread: crate::id::NodeId,
    pub kernel_stack_top: u64,
    pub lapic_id: u32,
    pub need_resched: u8,
    pub _pad: [u8; 3],
}

impl NodeBody for Cpu {
    const KIND: NodeKind = NodeKind::Cpu;
    const ZERO: Self = Cpu {
        current: crate::id::NodeId::NULL,
        idle_thread: crate::id::NodeId::NULL,
        kernel_stack_top: 0,
        lapic_id: 0,
        need_resched: 0,
        _pad: [0; 3],
    };
}

/// The capability container. `handles` is the derived index that makes a
/// syscall's authority check O(1) (invariant I8).
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Process {
    /// slot -> edge slab index of a `Holds` edge; 0 means the slot is empty.
    pub handles: [u32; HANDLE_SLOTS],
    /// Where to resume the search for a free slot. Zero means "from the
    /// start"; slot 0 is the null handle and is never allocated. Keeping the
    /// resting value at zero is what keeps `Graph::EMPTY` all-zero, and so
    /// what keeps the whole graph in `.bss` instead of the kernel image.
    pub next_slot_hint: u32,
    pub exit_code: i32,
}

impl NodeBody for Process {
    const KIND: NodeKind = NodeKind::Process;
    const ZERO: Self = Process { handles: [0; HANDLE_SLOTS], next_slot_hint: 0, exit_code: 0 };
}

impl core::fmt::Debug for Process {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let used = self.handles.iter().filter(|&&h| h != 0).count();
        f.debug_struct("Process").field("handles_used", &used).field("exit_code", &self.exit_code).finish()
    }
}

impl PartialEq for Process {
    fn eq(&self, other: &Self) -> bool {
        self.handles == other.handles && self.exit_code == other.exit_code
    }
}
impl Eq for Process {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ThreadState {
    /// Created but never started: no `Ready` in-edge, no `Waiting` out-edge.
    Inert = 0,
    Ready = 1,
    Running = 2,
    Blocked = 3,
    Dying = 4,
}

/// One IPC message, held on the sending and receiving threads rather than in a
/// queue. Synchronous rendezvous means a message exists only while exactly two
/// threads are looking at it, so there is nowhere else for it to live and no
/// way for a capability to be "in transit" inside a kernel object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Message {
    pub words: [u64; 8],
    /// The object a transferred capability names, or null.
    pub cap_target: crate::id::NodeId,
    /// The rights the sender is passing on, masked by what it held.
    pub cap_rights: u32,
    /// Slot the capability landed in on the receiving side, or 0.
    pub cap_slot: u32,
    pub has_cap: u8,
    pub _pad: [u8; 7],
}

impl Message {
    pub const ZERO: Message = Message {
        words: [0; 8],
        cap_target: crate::id::NodeId::NULL,
        cap_rights: 0,
        cap_slot: 0,
        has_cap: 0,
        _pad: [0; 7],
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Thread {
    /// Hot-hop cache of the `InSpace` target's page-table root (invariant I9).
    pub cr3: u64,
    pub kstack_top: u64,
    pub saved_rsp: u64,
    pub user_entry: u64,
    /// Hot-hop cache of the owning process (invariant I9).
    pub owner_proc: crate::id::NodeId,
    pub msg: Message,
    pub state: ThreadState,
    /// Set when the thing this thread was waiting on was destroyed under it.
    /// The kernel requeues such a thread with an error rather than letting it
    /// block forever on an object that no longer exists.
    pub wait_aborted: u8,
    pub _pad: [u8; 6],
}

impl NodeBody for Thread {
    const KIND: NodeKind = NodeKind::Thread;
    const ZERO: Self = Thread {
        cr3: 0,
        kstack_top: 0,
        saved_rsp: 0,
        user_entry: 0,
        owner_proc: crate::id::NodeId::NULL,
        msg: Message::ZERO,
        state: ThreadState::Inert,
        wait_aborted: 0,
        _pad: [0; 6],
    };
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct AddressSpace {
    /// Physical address of the page-table root. The tables themselves are a
    /// cache of `Maps` edges (invariant I5).
    pub pml4_phys: u64,
    /// Also the number of live entries in `ranges`.
    pub mapping_count: u32,
    pub is_kernel: u8,
    pub _pad: [u8; 3],
    /// Edge slab indices of this space's `Maps` edges, **sorted by virtual
    /// address**.
    ///
    /// The design admits (DESIGN 5.3, compromise 7) that a graph gives nothing
    /// for free when the question is keyed by an address: "which mapping covers
    /// this fault?" is a range query, and adjacency lists cannot answer one. So
    /// this is the side structure, and like every other derived index here it
    /// is maintained by the edge operations and audited by the checker
    /// (invariant I11).
    pub ranges: [u32; MAX_MAPPINGS_PER_SPACE],
}

impl NodeBody for AddressSpace {
    const KIND: NodeKind = NodeKind::AddressSpace;
    const ZERO: Self = AddressSpace {
        pml4_phys: 0,
        mapping_count: 0,
        is_kernel: 0,
        _pad: [0; 3],
        ranges: [0; MAX_MAPPINGS_PER_SPACE],
    };
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AddressSpace")
            .field("pml4_phys", &self.pml4_phys)
            .field("mappings", &self.mapping_count)
            .field("is_kernel", &self.is_kernel)
            .finish()
    }
}

impl PartialEq for AddressSpace {
    fn eq(&self, o: &Self) -> bool {
        self.pml4_phys == o.pml4_phys
            && self.mapping_count == o.mapping_count
            && self.is_kernel == o.is_kernel
    }
}
impl Eq for AddressSpace {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct MemFlags(pub u32);

impl MemFlags {
    pub const NONE: MemFlags = MemFlags(0);
    /// Device memory (framebuffer, MMIO): never handed back to the frame allocator.
    pub const DEVICE: MemFlags = MemFlags(1 << 0);
    /// Kernel-pinned: the reaper must not reclaim the frames.
    pub const PINNED: MemFlags = MemFlags(1 << 1);
    /// The object's pages are **not contiguous**, and may not exist yet.
    ///
    /// `phys_base` means nothing for such an object. Instead `frames_phys`
    /// points at a table of one physical address per page, zero where the page
    /// has never been touched. That table is not in the graph, for the same
    /// reason page tables are not: it is a dense array indexed by position, and
    /// a node per page would be a million nodes saying nothing (DESIGN 4.4).
    /// The graph records where it is so the reaper can walk it.
    pub const PAGED: MemFlags = MemFlags(1 << 2);
    pub const fn contains(self, o: MemFlags) -> bool {
        self.0 & o.0 == o.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct MemoryObject {
    /// First frame, for a contiguous object. Meaningless when `PAGED`.
    pub phys_base: u64,
    /// Frame table, for a `PAGED` object: `pages` entries of one physical
    /// address each, zero where the page is not yet backed.
    pub frames_phys: u64,
    pub pages: u32,
    pub flags: MemFlags,
    /// Hot-hop cache: number of `Maps` in-edges (invariant I9).
    pub map_count: u32,
    /// How many frames the frame table itself occupies.
    pub frames_pages: u32,
}

impl NodeBody for MemoryObject {
    const KIND: NodeKind = NodeKind::MemoryObject;
    const ZERO: Self = MemoryObject {
        phys_base: 0,
        frames_phys: 0,
        pages: 0,
        flags: MemFlags::NONE,
        map_count: 0,
        frames_pages: 0,
    };
}

impl MemoryObject {
    pub const fn is_paged(&self) -> bool {
        self.flags.contains(MemFlags::PAGED)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Endpoint {
    /// Monotonic source of badges for capabilities minted from this endpoint.
    pub badge_counter: u64,
}

impl NodeBody for Endpoint {
    const KIND: NodeKind = NodeKind::Endpoint;
    const ZERO: Self = Endpoint { badge_counter: 0 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum DeviceClass {
    None = 0,
    SerialConsole = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Device {
    pub class: DeviceClass,
    pub io_base: u16,
    pub irq: u8,
    pub _pad: [u8; 3],
}

impl NodeBody for Device {
    const KIND: NodeKind = NodeKind::Device;
    const ZERO: Self =
        Device { class: DeviceClass::None, io_base: 0, irq: 0, _pad: [0; 3] };
}

// ----------------------------------------------- endpoint marker traits ---
// These give invariant I3 (edge/endpoint compatibility) at *compile time* for
// every typed call site. The runtime table in `id::compatible` is only
// consulted on the generic path and by the checker.

/// May be owned by a `Process`.
pub trait ProcessOwnable: NodeBody {}
impl ProcessOwnable for Process {}
impl ProcessOwnable for Thread {}
impl ProcessOwnable for AddressSpace {}
impl ProcessOwnable for MemoryObject {}
impl ProcessOwnable for Endpoint {}

/// May be owned by a `Thread`. Only its own stack, in v1.
pub trait ThreadOwnable: NodeBody {}
impl ThreadOwnable for MemoryObject {}

/// May be owned by `Root` (that is: anything except another root).
pub trait RootOwnable: NodeBody {}
impl RootOwnable for Cpu {}
impl RootOwnable for Process {}
impl RootOwnable for Thread {}
impl RootOwnable for AddressSpace {}
impl RootOwnable for MemoryObject {}
impl RootOwnable for Endpoint {}
impl RootOwnable for Device {}

/// May be the target of a capability.
pub trait Holdable: NodeBody {}
impl Holdable for Root {}
impl Holdable for Process {}
impl Holdable for Thread {}
impl Holdable for AddressSpace {}
impl Holdable for MemoryObject {}
impl Holdable for Endpoint {}
impl Holdable for Device {}

/// May be waited on by a thread.
pub trait Waitable: NodeBody {}
impl Waitable for Endpoint {}
impl Waitable for Device {}

/// May be given a name. Anything, in v1.
pub trait Nameable: NodeBody {}
impl<B: NodeBody> Nameable for B {}

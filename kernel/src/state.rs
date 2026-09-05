//! The kernel's state: one graph, one frame allocator, one checker.
//!
//! `Graph::EMPTY` is all zeroes, so this static costs nothing in the image and
//! needs no initialisation pass. That is the property that lets the graph exist
//! before the allocator does (DESIGN 3.7).

use bramble_graph::body::*;
use bramble_graph::edge::{MapsAttr, Prot};
use bramble_graph::checker::Checker;
use bramble_graph::graph::{Graph, Ref};
use bramble_graph::id::NodeId;

use limine::file::File;
use limine::memory_map::{Entry, EntryType};

use crate::frames::{FrameAllocator, BitmapRegion, FRAME_SIZE};
use crate::sync::IrqLock;

/// Where the linker script puts the kernel image.
pub const KERNEL_IMAGE_BASE: u64 = 0xffff_ffff_8000_0000;

pub static GRAPH: IrqLock<Graph> = IrqLock::new(Graph::EMPTY);

/// The cpu node's id, cached outside any lock.
///
/// `BOOT` is an `IrqLock`, and taking one costs a `pushfq`, a `cli` and an
/// `sti`. That is cheap on real silicon and expensive under emulation, and the
/// scheduler and the IPC handoff both want this id on their hottest paths. It
/// is set once at boot and never changes, so a plain atomic is the honest
/// representation. Listed in the non-graph register (DESIGN 4.4) with the rest.
static CPU0: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[inline]
pub fn cpu0() -> NodeId {
    NodeId(CPU0.load(core::sync::atomic::Ordering::Relaxed))
}
pub static FRAMES: IrqLock<Option<FrameAllocator>> = IrqLock::new(None);
static CHECKER: IrqLock<Checker> = IrqLock::new(Checker::new());

/// Everything the checker can find wrong: the graph's own invariants, and the
/// one that needs hardware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inconsistency {
    Graph(bramble_graph::checker::Violation),
    PageTables(crate::vm::I5),
    /// A thread wrote past the bottom of its kernel stack.
    StackOverflow(NodeId),
}

/// Run every invariant check against the live graph, under one lock hold.
///
/// DESIGN 4.3: this is not optional tooling. A failure is a panic with a graph
/// dump, not a log line.
pub fn check_now() -> Result<(), Inconsistency> {
    let g = GRAPH.lock();
    let mut c = CHECKER.lock();
    c.check(&g).map_err(Inconsistency::Graph)?;
    crate::vm::check_page_tables(&g).map_err(Inconsistency::PageTables)?;
    crate::sched::check_stack_canaries(&g).map_err(Inconsistency::StackOverflow)?;
    Ok(())
}

/// Panic with a dump if any invariant is broken.
pub fn assert_consistent(context: &str) {
    if let Err(v) = check_now() {
        crate::println!();
        crate::cprintln!(crate::fb::ALERT, "checker failed during {}: {:?}", context, v);
        crate::dump::dump_graph_best_effort();
        panic!("invariant violated during {}", context);
    }
}

/// What boot handed to the graph, kept so later phases can find it again
/// without re-reading the bootloader's structures.
#[derive(Clone, Copy, Default)]
pub struct BootNodes {
    pub root: NodeId,
    pub cpu0: NodeId,
    pub console: NodeId,
    pub framebuffer: NodeId,
    pub bitmap: NodeId,
    pub kernel_image: NodeId,
    pub kernel_space: NodeId,
    pub phys_memory: NodeId,
}

pub static BOOT: IrqLock<BootNodes> = IrqLock::new(BootNodes {
    root: NodeId::NULL,
    cpu0: NodeId::NULL,
    console: NodeId::NULL,
    framebuffer: NodeId::NULL,
    bitmap: NodeId::NULL,
    kernel_image: NodeId::NULL,
    kernel_space: NodeId::NULL,
    phys_memory: NodeId::NULL,
});

fn pages_for(bytes: u64) -> u32 {
    bytes.div_ceil(FRAME_SIZE as u64) as u32
}

/// Create the boot graph: a root, a cpu, and a memory object for every region
/// of physical memory that is spoken for. Everything the frame allocator has
/// handed out that outlives the call is described by a node.
pub fn populate(
    entries: &[&Entry],
    modules: &[&File],
    bitmap: BitmapRegion,
    serial_io_base: u16,
) -> Result<(), bramble_graph::graph::GraphError> {
    let mut g = GRAPH.lock();
    let mut boot = BOOT.lock();
    let (total_frames, free_frames) = match FRAMES.lock().as_ref() {
        Some(fa) => (fa.total_frames() as u64, fa.free_frames() as u64),
        None => (0, 0),
    };

    let root = g.create_root()?;
    boot.root = root.id();

    let cpu = g.create_under_root(root, Cpu::ZERO)?;
    boot.cpu0 = cpu.id();
    CPU0.store(cpu.id().0, core::sync::atomic::Ordering::Relaxed);
    g.link_named(root, cpu, "cpu0")?;

    // The kernel image and any bootloader-owned regions the firmware told us
    // about. These are pinned: the reaper must never hand them back.
    for e in entries {
        if e.entry_type == EntryType::EXECUTABLE_AND_MODULES {
            let m = g.create_under_root(
                root,
                MemoryObject {
                    phys_base: e.base,
                    pages: pages_for(e.length),
                    flags: MemFlags::PINNED,
                    ..MemoryObject::ZERO
                },
            )?;
            if boot.kernel_image.is_null() {
                boot.kernel_image = m.id();
                g.link_named(root, m, "kernel-image")?;
            }
        }
    }

    // The framebuffer is device memory, not RAM: a memory object with a flag,
    // not a device node (DESIGN 4.1).
    for e in entries {
        if e.entry_type == EntryType::FRAMEBUFFER {
            let m = g.create_under_root(
                root,
                MemoryObject {
                    phys_base: e.base,
                    pages: pages_for(e.length),
                    flags: MemFlags::DEVICE,
                    ..MemoryObject::ZERO
                },
            )?;
            if boot.framebuffer.is_null() {
                boot.framebuffer = m.id();
                g.link_named(root, m, "framebuffer")?;
            }
        }
    }

    // The frame allocator's own bookkeeping. It is not in the graph, but the
    // memory it occupies is, which is what keeps the accounting honest.
    let bm = g.create_under_root(
        root,
        MemoryObject {
            phys_base: bitmap.phys,
            pages: bitmap.pages,
            flags: MemFlags::PINNED,
            ..MemoryObject::ZERO
        },
    )?;
    boot.bitmap = bm.id();
    g.link_named(root, bm, "frame-bitmap")?;

    for (i, f) in modules.iter().enumerate() {
        // Limine hands modules to us through the direct map, so the pointer is
        // virtual. A MemoryObject records physical addresses.
        let phys = (f.addr() as u64).saturating_sub(crate::paging::hhdm());
        let m = g.create_under_root(
            root,
            MemoryObject {
                phys_base: phys,
                pages: pages_for(f.size()),
                flags: MemFlags::PINNED,
                ..MemoryObject::ZERO
            },
        )?;
        // Name the first few modules; long paths need a post-v1 string table.
        if i < 4 {
            let name = short_name(f);
            let _ = g.link_named(root, m, name);
        }
    }

    // The kernel's own address space. Its two mappings are recorded as edges
    // like any other, but they are exempt from the checker's page-table walk
    // (invariant I5'): verifying a direct map of all of RAM is O(RAM) and
    // proves nothing. The tables themselves are the bootloader's, which live in
    // memory the frame allocator never hands out.
    let phys = g.create_under_root(
        root,
        MemoryObject {
            phys_base: 0,
            pages: total_frames as u32,
            flags: MemFlags::PINNED,
            ..MemoryObject::ZERO
        },
    )?;
    boot.phys_memory = phys.id();
    g.link_named(root, phys, "physical-memory")?;

    let kspace = g.create_under_root(
        root,
        AddressSpace {
            pml4_phys: crate::paging::active_pml4(),
            is_kernel: 1,
            ..AddressSpace::ZERO
        },
    )?;
    boot.kernel_space = kspace.id();
    crate::paging::set_kernel_pml4(crate::paging::active_pml4());
    g.link_named(root, kspace, "kernel-space")?;
    g.link_maps(
        kspace,
        phys,
        MapsAttr {
            vaddr: crate::paging::hhdm(),
            len_pages: total_frames as u32,
            off_pages: 0,
            prot: Prot::READ.union(Prot::WRITE),
        },
    )?;
    if let Some(img) = g.typed::<MemoryObject>(boot.kernel_image) {
        let pages = g.body(img).map(|b| b.pages).unwrap_or(0);
        g.link_maps(
            kspace,
            img,
            MapsAttr {
                vaddr: KERNEL_IMAGE_BASE,
                len_pages: pages,
                off_pages: 0,
                prot: Prot::READ.union(Prot::WRITE).union(Prot::EXEC),
            },
        )?;
    }

    let dev = g.create_under_root(
        root,
        Device { class: DeviceClass::SerialConsole, io_base: serial_io_base, irq: 4, _pad: [0; 3] },
    )?;
    boot.console = dev.id();
    g.link_named(root, dev, "console")?;

    // Record what the allocator knows, so a dump carries the non-graph
    // register alongside the graph (DESIGN 4.4).
    let r: Ref<Root> = root;
    if let Some(b) = g.body_mut(r) {
        b.total_frames = total_frames;
        b.free_frames = free_frames;
    }
    Ok(())
}

/// The last path component of a module, truncated to what fits inline.
fn short_name(f: &File) -> &str {
    let bytes = f.path().to_bytes();
    let start = bytes.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    let slice = &bytes[start..];
    let slice = &slice[..slice.len().min(bramble_graph::limits::MAX_NAME_LEN)];
    core::str::from_utf8(slice).unwrap_or("module")
}

/// Update the root's copy of the frame counts. Cheap; called after allocation
/// bursts so the dump does not lie.
pub fn refresh_frame_counts() {
    let mut g = GRAPH.lock();
    let root = match g.root() {
        Some(r) => r,
        None => return,
    };
    let (total, free) = match FRAMES.lock().as_ref() {
        Some(fa) => (fa.total_frames() as u64, fa.free_frames() as u64),
        None => return,
    };
    if let Some(b) = g.body_mut(root) {
        b.total_frames = total;
        b.free_frames = free;
    }
}
